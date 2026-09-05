//! Bounded positive observations, never a replay-safety decision.
//! ACP lifecycle statuses are reports from the harness, not execution receipts.

use std::sync::{Arc, Mutex};

use agentkit_acp::{SessionUpdate, ToolCallStatus};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationSource {
    #[default]
    Unknown,
    AcpNotifications,
}

/// False means only "not observed". Completion does not imply success,
/// rollback, a committed effect, or permission to repeat an operation.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PossibleEffects {
    pub source: ObservationSource,
    pub assistant_output_observed: bool,
    pub tool_emission_observed: bool,
    pub tool_execution_start_reported: bool,
    pub tool_execution_completion_reported: bool,
    pub observation_incomplete: bool,
}

impl Default for PossibleEffects {
    fn default() -> Self {
        Self {
            source: ObservationSource::Unknown,
            assistant_output_observed: false,
            tool_emission_observed: false,
            tool_execution_start_reported: false,
            tool_execution_completion_reported: false,
            // No failure path proves complete observation of external activity.
            observation_incomplete: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Observations(Arc<Mutex<PossibleEffects>>);

impl Observations {
    pub(crate) fn snapshot(&self) -> PossibleEffects {
        self.0.lock().map(|value| *value).unwrap_or_default()
    }

    pub(crate) fn record(&self, update: &SessionUpdate) {
        let Ok(mut effects) = self.0.lock() else {
            return;
        };
        effects.source = ObservationSource::AcpNotifications;
        let status = match update {
            SessionUpdate::AgentMessageChunk(_) => {
                effects.assistant_output_observed = true;
                None
            }
            SessionUpdate::ToolCall(call) => {
                effects.tool_emission_observed = true;
                Some(call.status)
            }
            SessionUpdate::ToolCallUpdate(update) => {
                effects.tool_emission_observed = true;
                update.fields.status
            }
            _ => None,
        };
        match status {
            Some(ToolCallStatus::InProgress) => effects.tool_execution_start_reported = true,
            Some(ToolCallStatus::Completed) => {
                // Do not manufacture a start observation from completion.
                // Failed can mean pre-execution denial, not completed execution.
                effects.tool_execution_completion_reported = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(value: serde_json::Value) -> SessionUpdate {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn observations_are_monotonic_and_do_not_infer_execution() {
        let observations = Observations::default();
        assert_eq!(observations.snapshot(), PossibleEffects::default());
        observations.record(&update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "secret output"}
        })));
        assert!(observations.snapshot().assistant_output_observed);
        observations.record(&update(json!({
            "sessionUpdate": "tool_call", "toolCallId": "private-id",
            "title": "private command", "status": "pending",
            "rawInput": {"secret": "private arguments"}
        })));
        let emitted = observations.snapshot();
        assert!(emitted.tool_emission_observed);
        assert!(!emitted.tool_execution_start_reported);
        observations.record(&update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "private-id",
            "status": "completed", "rawOutput": "private result"
        })));
        let completed = observations.snapshot();
        assert!(completed.tool_execution_completion_reported);
        assert!(!completed.tool_execution_start_reported);
        observations.record(&update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "other",
            "status": "in_progress"
        })));
        let effects = observations.snapshot();
        assert!(effects.tool_execution_start_reported);
        assert!(effects.tool_execution_completion_reported);
        assert!(effects.observation_incomplete);
        let envelope = serde_json::to_string(&effects).unwrap();
        assert!(!envelope.contains("secret"));
        assert!(!envelope.contains("private"));
        assert!(envelope.len() < 1024);
    }

    #[test]
    fn failed_status_does_not_prove_execution_completed() {
        let observations = Observations::default();
        observations.record(&update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "denied",
            "status": "failed"
        })));
        let effects = observations.snapshot();
        assert!(effects.tool_emission_observed);
        assert!(!effects.tool_execution_start_reported);
        assert!(!effects.tool_execution_completion_reported);
        assert!(effects.observation_incomplete);
    }

    #[test]
    fn malformed_or_payload_bearing_metadata_is_rejected() {
        let mut value = serde_json::to_value(PossibleEffects::default()).unwrap();
        value["tool_arguments"] = json!("secret");
        assert!(serde_json::from_value::<PossibleEffects>(value).is_err());
        let mut value = serde_json::to_value(PossibleEffects::default()).unwrap();
        value["assistant_output_observed"] = json!("false");
        assert!(serde_json::from_value::<PossibleEffects>(value).is_err());
    }
}
