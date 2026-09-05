//! Bounded positive observations, never a replay-safety decision.
//! ACP lifecycle statuses are reports from the harness, not execution receipts.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use agentkit_core::{Delta, Part, PartId, PartKind};
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};

use agentkit_acp::{SessionUpdate, ToolCallStatus};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationSource {
    #[default]
    Unknown,
    AcpNotifications,
    /// Cumulative observations during this live root owner's lifetime only.
    LocalSession,
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
    #[serde(deserialize_with = "incomplete_observation")]
    pub observation_incomplete: bool,
}

fn incomplete_observation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    if bool::deserialize(deserializer)? {
        Ok(true)
    } else {
        Err(serde::de::Error::custom(
            "failure observation must remain incomplete",
        ))
    }
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

const MAX_TRACKED_PARTS: usize = 128;
const MAX_PART_ID_BYTES: usize = 256;

#[derive(Debug, Default)]
struct ObservationState {
    effects: PossibleEffects,
    // Transient classification only; never serialized into diagnostics.
    assistant_parts: HashSet<PartId>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Observations(Arc<Mutex<ObservationState>>);

impl Observations {
    pub(crate) fn local_session() -> Self {
        Self(Arc::new(Mutex::new(ObservationState {
            effects: PossibleEffects {
                source: ObservationSource::LocalSession,
                ..Default::default()
            },
            ..Default::default()
        })))
    }

    pub(crate) fn invocation_started(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.effects.tool_execution_start_reported = true;
        }
    }

    pub(crate) fn invocation_completed(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.effects.tool_execution_completion_reported = true;
        }
    }

    fn observe_local(&self, event: &AgentEvent) {
        let Ok(mut state) = self.0.lock() else { return };
        match event {
            AgentEvent::ToolCallRequested(_) => state.effects.tool_emission_observed = true,
            AgentEvent::ContentDelta(delta) => match delta {
                Delta::BeginPart { part_id, kind } => {
                    state.assistant_parts.remove(part_id);
                    if *kind == PartKind::ToolCall {
                        state.effects.tool_emission_observed = true;
                    }
                    if matches!(
                        kind,
                        PartKind::Text | PartKind::Media | PartKind::File | PartKind::Structured
                    ) && part_id.0.len() <= MAX_PART_ID_BYTES
                        && state.assistant_parts.len() < MAX_TRACKED_PARTS
                    {
                        state.assistant_parts.insert(part_id.clone());
                    }
                }
                Delta::AppendText { part_id, chunk } => {
                    if !chunk.is_empty() && state.assistant_parts.contains(part_id) {
                        state.effects.assistant_output_observed = true;
                    }
                }
                Delta::AppendBytes { part_id, chunk } => {
                    if !chunk.is_empty() && state.assistant_parts.contains(part_id) {
                        state.effects.assistant_output_observed = true;
                    }
                }
                Delta::ReplaceStructured { part_id, .. } => {
                    if state.assistant_parts.contains(part_id) {
                        state.effects.assistant_output_observed = true;
                    }
                }
                Delta::CommitPart { part } => {
                    match part {
                        Part::Text(text) if !text.text.is_empty() => {
                            state.effects.assistant_output_observed = true
                        }
                        Part::Media(_) | Part::File(_) | Part::Structured(_) => {
                            state.effects.assistant_output_observed = true
                        }
                        Part::ToolCall(_) => state.effects.tool_emission_observed = true,
                        _ => {}
                    }
                    // CommitPart carries no PartId. Forget classifications rather
                    // than risk applying a stale kind to a later reused id.
                    state.assistant_parts.clear();
                }
                Delta::SetMetadata { .. } => {}
            },
            AgentEvent::TurnStarted { .. }
            | AgentEvent::TurnFinished(_)
            | AgentEvent::ResponseAttemptSuperseded => {
                // Presentation/turn boundaries cannot erase effects, particularly
                // when a detached invocation spans more than one prompt.
                state.assistant_parts.clear();
            }
            // ToolResultReceived includes synthetic permission/cancellation
            // results. Only actual invocation boundaries report execution.
            _ => {}
        }
    }

    pub(crate) fn snapshot(&self) -> PossibleEffects {
        self.0.lock().map(|value| value.effects).unwrap_or_default()
    }

    pub(crate) fn record(&self, update: &SessionUpdate) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        let effects = &mut state.effects;
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

impl LoopObserver for Observations {
    fn handle_event(&self, event: ObservedEvent) {
        self.observe_local(&event.event);
    }
}

#[cfg(test)]
pub(crate) fn isolated_test(name: &str) -> bool {
    if std::env::var("KIT_EFFECTS_TEST_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("KIT_EFFECTS_TEST_CHILD", name)
        .env("HOME", home.path())
        .env_remove(crate::events::EVENTS_ENV)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated effects test failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[cfg(test)]
pub(crate) fn test_record(session_id: &str) -> serde_json::Value {
    let directory = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(".kit/errors")
        .join(session_id);
    let mut files: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|file| file.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    serde_json::from_slice(&std::fs::read(files.last().unwrap()).unwrap()).unwrap()
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
        let mut complete = serde_json::to_value(PossibleEffects::default()).unwrap();
        complete["observation_incomplete"] = json!(false);
        assert!(serde_json::from_value::<PossibleEffects>(complete).is_err());
        let mut value = serde_json::to_value(PossibleEffects::default()).unwrap();
        value["tool_arguments"] = json!("secret");
        assert!(serde_json::from_value::<PossibleEffects>(value).is_err());
        let mut value = serde_json::to_value(PossibleEffects::default()).unwrap();
        value["assistant_output_observed"] = json!("false");
        assert!(serde_json::from_value::<PossibleEffects>(value).is_err());
    }
    #[test]
    fn local_content_classification_is_bounded_and_never_invents_output() {
        let observations = Observations::local_session();
        let emit = |delta| observations.observe_local(&AgentEvent::ContentDelta(delta));
        let id = PartId::new("private-part");
        emit(Delta::BeginPart {
            part_id: id.clone(),
            kind: PartKind::Reasoning,
        });
        emit(Delta::AppendText {
            part_id: id.clone(),
            chunk: "private reasoning".into(),
        });
        emit(Delta::BeginPart {
            part_id: id.clone(),
            kind: PartKind::ToolCall,
        });
        emit(Delta::AppendText {
            part_id: id.clone(),
            chunk: "private arguments".into(),
        });
        assert!(!observations.snapshot().assistant_output_observed);
        assert!(observations.snapshot().tool_emission_observed);
        assert!(!observations.snapshot().tool_execution_start_reported);
        emit(Delta::BeginPart {
            part_id: id.clone(),
            kind: PartKind::Text,
        });
        assert!(!observations.snapshot().assistant_output_observed);
        emit(Delta::AppendText {
            part_id: id,
            chunk: "private answer".into(),
        });
        assert!(observations.snapshot().assistant_output_observed);
        observations.observe_local(&AgentEvent::ResponseAttemptSuperseded);
        observations.observe_local(&AgentEvent::TurnStarted {
            session_id: "session".into(),
            turn_id: "next".into(),
        });
        assert!(observations.snapshot().assistant_output_observed);
        let encoded = serde_json::to_string(&observations.snapshot()).unwrap();
        assert!(!encoded.contains("private"));
        assert_eq!(
            observations.snapshot().source,
            ObservationSource::LocalSession
        );
        assert!(observations.snapshot().observation_incomplete);

        let bounded = Observations::local_session();
        for index in 0..MAX_TRACKED_PARTS + 3 {
            bounded.observe_local(&AgentEvent::ContentDelta(Delta::BeginPart {
                part_id: format!("part-{index}").into(),
                kind: PartKind::Text,
            }));
        }
        assert_eq!(
            bounded.0.lock().unwrap().assistant_parts.len(),
            MAX_TRACKED_PARTS
        );
        bounded.observe_local(&AgentEvent::ContentDelta(Delta::AppendText {
            part_id: format!("part-{}", MAX_TRACKED_PARTS + 2).into(),
            chunk: "unclassified".into(),
        }));
        let huge = PartId::new("x".repeat(MAX_PART_ID_BYTES + 1));
        bounded.observe_local(&AgentEvent::ContentDelta(Delta::BeginPart {
            part_id: huge.clone(),
            kind: PartKind::Text,
        }));
        bounded.observe_local(&AgentEvent::ContentDelta(Delta::AppendText {
            part_id: huge,
            chunk: "unclassified".into(),
        }));
        assert!(!bounded.snapshot().assistant_output_observed);
        bounded.observe_local(&AgentEvent::ContentDelta(Delta::CommitPart {
            part: Part::text("commit-only output"),
        }));
        assert!(bounded.snapshot().assistant_output_observed);
        assert!(bounded.0.lock().unwrap().assistant_parts.is_empty());
    }

    #[test]
    fn synthesized_results_are_not_invocation_receipts_and_sessions_are_isolated() {
        let first = Observations::local_session();
        let second = Observations::local_session();
        first.observe_local(&AgentEvent::ToolResultReceived(
            agentkit_core::ToolResultPart::error(
                "denied",
                agentkit_core::ToolOutput::text("denied"),
            ),
        ));
        assert!(!first.snapshot().tool_execution_start_reported);
        assert!(!first.snapshot().tool_execution_completion_reported);
        let background = first.clone();
        background.invocation_started();
        first.observe_local(&AgentEvent::TurnStarted {
            session_id: "session".into(),
            turn_id: "next".into(),
        });
        background.invocation_completed();
        assert!(first.snapshot().tool_execution_start_reported);
        assert!(first.snapshot().tool_execution_completion_reported);
        assert!(!second.snapshot().tool_execution_start_reported);
    }
}
