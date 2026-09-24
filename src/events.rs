//! Ephemeral stderr diagnostics for session attachment, runtime health, and
//! subagent lifecycle. Tool cards are projected through canonical ACP events.

use std::{
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// Prefix that distinguishes an event line from a diagnostic line. The
/// leading control character cannot appear in ordinary log text.
pub const EVENT_MARKER: &str = "\u{1}kit-runtime\u{1}";

/// Environment variable that turns emission on for a `serve` process.
pub const EVENTS_ENV: &str = "KIT_RUNTIME_EVENTS";

/// One runtime event sent privately to the terminal client.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// Process-wide diagnostic transport lease/reset.
    RunletTransport { available: bool },
    /// Process-wide durability state, independent of the active ACP session.
    StorageStatus { pending: bool, exhausted: bool },
    /// A persisted ACP session was opened by the child runtime.
    SessionStarted { session_id: String },
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
    SubagentUsage {
        id: String,
        used: u64,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<agent_client_protocol::schema::v2::Cost>,
    },
    /// Steering support advertised by the child's negotiated ACP connection.
    SubagentCapabilities {
        id: String,
        generation: u64,
        can_steer: bool,
    },
    /// A child ACP update relevant to its roster excerpt.
    SubagentActivity {
        id: String,
        activity: SubagentActivity,
    },
    /// Every observable strict descendant of an ancestor should be removed.
    SubagentDescendantsRemoved { ancestor_id: String },
}

/// Partial activity updates preserve omitted fields from earlier notifications.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentActivity {
    Tool {
        id: String,
        title: Option<String>,
        running: Option<bool>,
    },
    Plan {
        entry: Option<String>,
    },
    Title {
        title: Option<String>,
    },
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
            Self::SubagentStateChanged { .. }
                | Self::SubagentUsage { .. }
                | Self::SubagentActivity { .. }
                | Self::SubagentCapabilities { .. }
                | Self::SubagentDescendantsRemoved { .. }
        )
    }
}

/// Whether this process should emit runtime events.
#[must_use]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os(EVENTS_ENV).is_some())
}

/// Enqueues one event without waiting for stderr. Loss disables the transport;
/// its explicit reset (or the client lease on a stalled sink) invalidates runtime status.
pub fn emit(event: &RuntimeEvent) {
    if !enabled() {
        return;
    }
    if let Some(transport) = crate::diagnostic_transport::global() {
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
    serde_json::from_str(body).ok()
}

/// Milliseconds since the Unix epoch, saturating at zero on a broken clock.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
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
    use serde_json::json;
    use std::io::{self, Write};

    use super::{
        EVENT_MARKER, GenerationOutcome, HarnessVendor, RuntimeEvent, SubagentStatus, parse,
        test_support::write_event,
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
        }
    }

    #[test]
    fn reads_back_an_emitted_event_line() {
        let event = RuntimeEvent::CompactionStarted {
            reason: "test".into(),
            at: 7,
        };
        let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap());
        let parsed = parse(&line).expect("event round trips");
        assert_eq!(parsed, event);
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
    fn subagent_inspection_round_trips_and_forwards_from_children() {
        for event in [
            RuntimeEvent::SubagentCapabilities {
                id: "nested-child".into(),
                generation: 7,
                can_steer: false,
            },
            RuntimeEvent::SubagentCapabilities {
                id: "nested-child".into(),
                generation: 7,
                can_steer: true,
            },
        ] {
            let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap());
            assert_eq!(parse(&line), Some(event.clone()));
            assert!(event.forward_from_child());
        }
    }

    #[test]
    fn subagent_activity_round_trips_and_forwards_from_children() {
        for activity in [
            super::SubagentActivity::Tool {
                id: "tool".into(),
                title: None,
                running: Some(false),
            },
            super::SubagentActivity::Plan {
                entry: Some("Implement parser".into()),
            },
            super::SubagentActivity::Title { title: None },
        ] {
            let event = RuntimeEvent::SubagentActivity {
                id: "scout".into(),
                activity,
            };
            let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap());
            assert_eq!(parse(&line), Some(event.clone()));
            assert!(event.forward_from_child());
        }
    }

    #[test]
    fn subagent_usage_round_trips_and_forwards_from_children() {
        let usage = RuntimeEvent::SubagentUsage {
            id: "s-child".into(),
            used: 12_345,
            size: 200_000,
            cost: None,
        };
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["event"], "subagent_usage");
        let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&usage).unwrap());
        let parsed = parse(&line).unwrap();
        assert_eq!(parsed, usage);
        assert!(parsed.forward_from_child());
    }

    #[test]
    fn subagent_cost_events_accept_legacy_and_round_trip_current_reports() {
        let legacy = json!({"event": "subagent_usage", "id": "child", "used": 1, "size": 2});
        let old: RuntimeEvent = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(old).unwrap(), legacy);
        let mut current = legacy;
        current["cost"] = json!({"amount": 1.25, "currency": "USD"});
        let event: RuntimeEvent = serde_json::from_value(current.clone()).unwrap();
        assert!(event.forward_from_child());
        assert_eq!(serde_json::to_value(event).unwrap(), current);
        current["cost"]["amount"] = json!("invalid");
        assert!(serde_json::from_value::<RuntimeEvent>(current).is_err());
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
        assert_eq!(parsed, event);
        assert!(matches!(parsed, RuntimeEvent::CompactionStarted { .. }));
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
}

#[cfg(test)]
pub(crate) mod test_support;
