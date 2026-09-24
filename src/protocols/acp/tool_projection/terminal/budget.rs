//! Cumulative admission, not a per-call rate limit: a stalled SDK transport can
//! retain only this much extra stream payload for the subscription's lifetime.
//! Invocation/terminal lifecycle delivery is never charged against this budget.
use super::super::Update;
use serde_json::{Map, Value};

pub(super) const MAX_BYTES: usize = 1024 * 1024;
pub(super) const MAX_CHUNKS: usize = 128;

#[derive(Default)]
pub(in super::super) struct State {
    pub(in super::super) running: bool,
    incomplete: bool,
}

impl State {
    pub(in super::super) fn running() -> Self {
        Self {
            running: true,
            incomplete: false,
        }
    }
}

pub(in super::super) struct Budget {
    bytes: usize,
    chunks: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            bytes: MAX_BYTES,
            chunks: MAX_CHUNKS,
        }
    }
}

impl Budget {
    pub(in super::super) fn admit(&mut self, update: &mut Update, state: &mut State) -> bool {
        let Some(patch) = update.patch.as_mut().and_then(Value::as_object_mut) else {
            return true;
        };
        match patch.get("sessionUpdate").and_then(Value::as_str) {
            Some("terminal_output_chunk") => {
                if state.incomplete {
                    return false;
                }
                let bytes = patch
                    .get("data")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
                if self.chunks > 0 && bytes <= self.bytes {
                    self.bytes -= bytes;
                    self.chunks -= 1;
                } else {
                    state.incomplete = true;
                    // Replace the first omitted chunk with one small, explicit
                    // notice; suppress the rest without dropping the exit.
                    *patch = Map::from_iter([
                        ("sessionUpdate".into(), Value::from("terminal_update")),
                        (
                            "terminalId".into(),
                            patch.get("terminalId").cloned().unwrap_or(Value::Null),
                        ),
                        (
                            "_meta".into(),
                            Value::Object(Map::from_iter([(
                                "kit/outputIncomplete".into(),
                                Value::from(true),
                            )])),
                        ),
                    ]);
                }
            }
            Some("terminal_update") => {
                // Child terminals can carry replacement output snapshots, not
                // just chunks. Charge those against the same lifetime budget.
                if let Some(bytes) = patch
                    .get("output")
                    .and_then(|output| output.get("data"))
                    .and_then(Value::as_str)
                    .map(str::len)
                {
                    if !state.incomplete && self.chunks > 0 && bytes <= self.bytes {
                        self.bytes -= bytes;
                        self.chunks -= 1;
                    } else {
                        patch.remove("output");
                        state.incomplete = true;
                        patch.insert(
                            "_meta".into(),
                            Value::Object(Map::from_iter([(
                                "kit/outputIncomplete".into(),
                                Value::from(true),
                            )])),
                        );
                    }
                }
                // Command/cwd are optional, variable-size metadata. Charge the
                // worst-case JSON escape expansion too, across all shell calls.
                for key in ["command", "cwd"] {
                    let bytes = patch
                        .get(key)
                        .and_then(Value::as_str)
                        .map_or(0, str::len)
                        .saturating_mul(6);
                    if bytes <= self.bytes {
                        self.bytes -= bytes;
                    } else {
                        patch.remove(key);
                    }
                }
                // ACP metadata replaces the prior object. Once output admission
                // loses bytes, later clears/exit metadata cannot hide that fact.
                if state.incomplete {
                    let metadata = patch
                        .entry("_meta")
                        .or_insert_with(|| Value::Object(Map::new()));
                    if !metadata.is_object() {
                        *metadata = Value::Object(Map::new());
                    }
                    if let Some(metadata) = metadata.as_object_mut() {
                        metadata.insert("kit/outputIncomplete".into(), Value::from(true));
                    }
                }
            }
            _ => {}
        }
        true
    }
}
