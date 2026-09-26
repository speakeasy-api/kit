//! Agent-owned terminals are v2 only. Frames share the bounded invocation bus.
use super::{MAX_ID, Update, publish, routes};

pub(super) const MAX_COMMAND_BYTES: usize = 64 * 1024;
use agentkit_tools_core::ToolRequest;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value};
use std::path::Path;

mod budget;
pub(super) use budget::{Budget, State};

/// A reader retains only routing IDs, never command text or accumulated output.
#[derive(Clone)]
pub(crate) struct Output {
    session: String,
    call: String,
}

impl Output {
    fn publish(&self, patch: Value) {
        publish(Update {
            session: self.session.clone(),
            call: self.call.clone(),
            start: None,
            patch: Some(patch),
            ok: false,
        });
    }

    pub(crate) fn chunk(&self, bytes: &[u8]) {
        // The real shell reader uses this same bounded buffer size. Keep the
        // boundary safe for other callers too, including non-UTF-8 output.
        for bytes in bytes.chunks(8192) {
            self.publish(Value::Object(Map::from_iter([
                ("sessionUpdate".into(), Value::from("terminal_output_chunk")),
                ("terminalId".into(), Value::from(self.call.clone())),
                ("data".into(), Value::from(STANDARD.encode(bytes))),
            ])));
        }
    }

    fn exited(&self, code: Option<i32>) {
        self.publish(Value::Object(Map::from_iter([
            ("sessionUpdate".into(), Value::from("terminal_update")),
            ("terminalId".into(), Value::from(self.call.clone())),
            (
                "exitStatus".into(),
                Value::Object(Map::from_iter([(
                    "exitCode".into(),
                    code.and_then(|code| u32::try_from(code).ok())
                        .map_or(Value::Null, Value::from),
                )])),
            ),
        ])));
    }
}

/// Dropped before Observed terminalizes the call, also on errors/cancellation.
pub(crate) struct Terminal(Option<Output>);

impl Terminal {
    pub(crate) fn start(request: &ToolRequest, command: &str, cwd: &Path) -> Self {
        if routes()
            .senders(&request.session_id.0)
            .is_none_or(|(_, v2)| v2.receiver_count() == 0)
            || request.session_id.0.len() > MAX_ID
            || request.call_id.0.len() > MAX_ID
            || !request.call_id.0.contains(":compose:")
        {
            return Self(None);
        }
        let output = Output {
            session: request.session_id.0.clone(),
            call: request.call_id.0.clone(),
        };
        let mut patch = Value::Object(Map::from_iter([
            ("sessionUpdate".into(), Value::from("terminal_update")),
            ("terminalId".into(), Value::from(output.call.clone())),
        ]));
        // Omit oversized metadata rather than retaining or truncating commands.
        if command.len() <= MAX_COMMAND_BYTES {
            patch["command"] = Value::from(command);
        }
        if cwd.is_absolute()
            && let Some(cwd) = cwd.to_str().filter(|cwd| cwd.len() <= super::MAX_PATH)
        {
            patch["cwd"] = Value::from(cwd);
        }
        output.publish(patch);
        output.publish(Value::Object(Map::from_iter([
            ("sessionUpdate".into(), Value::from("tool_call_update")),
            ("toolCallId".into(), Value::from(output.call.clone())),
            (
                "content".into(),
                Value::Array(vec![Value::Object(Map::from_iter([
                    ("type".into(), Value::from("terminal")),
                    ("terminalId".into(), Value::from(output.call.clone())),
                ]))]),
            ),
        ])));
        Self(Some(output))
    }

    pub(crate) fn output(&self) -> Option<Output> {
        self.0.clone()
    }

    pub(crate) fn finish(mut self, code: Option<i32>) {
        if let Some(output) = self.0.take() {
            output.exited(code);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(output) = self.0.take() {
            // An empty exit object marks an exited terminal without inventing
            // an exit code for cancellation, timeout, I/O errors or unwind.
            output.exited(None);
        }
    }
}
