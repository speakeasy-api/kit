use agentkit_core::{Delta, PartId, PartKind};
use agentkit_loop::{ModelTurnEvent, SessionConfig};

pub(crate) const SESSION_METADATA_KEY: &str = "kit.internal.acp_v2_response_attempt_replacement";
const MARKER_PART_ID: &str = "\0kit.response-attempt-replacement";

pub(crate) fn enable(config: &mut SessionConfig) {
    config
        .metadata
        .insert(SESSION_METADATA_KEY.into(), serde_json::Value::Bool(true));
}

pub(crate) fn enabled(config: &SessionConfig) -> bool {
    config
        .metadata
        .get(SESSION_METADATA_KEY)
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

pub(crate) fn marker_event() -> ModelTurnEvent {
    ModelTurnEvent::Delta(Delta::BeginPart {
        part_id: PartId::new(MARKER_PART_ID),
        kind: PartKind::Custom,
    })
}

pub(crate) fn is_marker(delta: &Delta) -> bool {
    matches!(
        delta,
        Delta::BeginPart { part_id, kind: PartKind::Custom }
            if part_id.0 == MARKER_PART_ID
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_reserved_custom_begin_part() {
        let ModelTurnEvent::Delta(delta) = marker_event() else {
            panic!("marker must be a delta");
        };
        assert!(is_marker(&delta));
    }
}
