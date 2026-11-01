use super::*;
use agentkit_core::{MetadataMap, SessionId};
use agentkit_loop::MutationPoint;

fn started() -> AgentEvent {
    AgentEvent::MutationStarted {
        session_id: SessionId::from("session"),
        turn_id: None,
        mutator: "automatic-compaction".into(),
        point: MutationPoint::AfterTurnEnded,
    }
}

fn finished(metadata: MetadataMap) -> AgentEvent {
    AgentEvent::MutationFinished {
        session_id: SessionId::from("session"),
        turn_id: None,
        mutator: "automatic-compaction".into(),
        dirty: true,
        metadata,
    }
}

#[test]
fn summary_is_appended_once_then_completed_without_replacement() {
    let projection = Projection::default();
    let start = projection.observe(&started());
    let SessionUpdate::CompactionUpdate(start) = &start[0] else {
        panic!("expected compaction notification")
    };
    assert_eq!(start.status, CompactionStatus::InProgress);
    let mut metadata = MetadataMap::new();
    metadata.insert("summary".into(), "checkpoint".into());
    let event = finished(metadata);
    let updates = projection.clone().observe(&event);
    assert_eq!(updates.len(), 2);
    let SessionUpdate::CompactionSummaryChunk(chunk) = &updates[0] else {
        panic!("expected compaction notification")
    };
    assert_eq!(chunk.compaction_id, start.compaction_id);
    let SessionUpdate::CompactionUpdate(done) = &updates[1] else {
        panic!("expected compaction notification")
    };
    assert_eq!(done.compaction_id, start.compaction_id);
    assert_eq!(done.status, CompactionStatus::Completed);
    assert!(done.summary.is_undefined());
    assert!(projection.observe(&event).is_empty());
    let next = projection.observe(&started());
    let SessionUpdate::CompactionUpdate(next) = &next[0] else {
        panic!("expected compaction notification")
    };
    assert_ne!(next.compaction_id, start.compaction_id);
    for update in updates {
        let value = serde_json::to_value(update).expect("serialize compaction update");
        let _: agentkit_acp::v2::wire::SessionUpdate =
            serde_json::from_value(value).expect("v2 compaction contract");
    }
}

#[test]
fn failure_and_cancellation_do_not_publish_summaries() {
    for cancelled in [false, true] {
        let projection = Projection::default();
        projection.observe(&started());
        let mut metadata = MetadataMap::new();
        metadata.insert("error".into(), "stopped".into());
        metadata.insert("cancelled".into(), cancelled.into());
        metadata.insert("summary".into(), "not installed".into());
        let updates = projection.observe(&finished(metadata));
        assert_eq!(updates.len(), 1);
        let SessionUpdate::CompactionUpdate(done) = &updates[0] else {
            panic!("expected compaction notification")
        };
        assert_eq!(
            done.status,
            if cancelled {
                CompactionStatus::Cancelled
            } else {
                CompactionStatus::Failed
            }
        );
    }
}
