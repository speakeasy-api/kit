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
                        ("terminalId".into(), Value::from(update.call.clone())),
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
            }
            _ => {}
        }
        true
    }
}
