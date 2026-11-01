//! Projects the foreground compactor lifecycle, never speculative preparation.
#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]
mod tests;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use agent_client_protocol::schema::v1::{
    CompactionStatus, CompactionSummaryChunk, CompactionUpdate, ContentBlock, SessionUpdate,
};
use agentkit_loop::AgentEvent;

/// A loop runs one mutator at a time. Clones share only the active lifecycle ID;
/// finishing consumes it, so duplicate terminal events cannot repeat summaries.
#[derive(Clone, Default)]
pub(super) struct Projection(Arc<AtomicU64>);

impl Projection {
    pub(super) fn observe(&self, event: &AgentEvent) -> Vec<SessionUpdate> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        match event {
            AgentEvent::MutationStarted { mutator, .. } if mutator == "automatic-compaction" => {
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                self.0.store(id, Ordering::Release);
                vec![SessionUpdate::CompactionUpdate(CompactionUpdate::new(
                    format!("compaction-{id}"),
                    CompactionStatus::InProgress,
                ))]
            }
            AgentEvent::MutationFinished {
                mutator, metadata, ..
            } if mutator == "automatic-compaction" => {
                let id = self.0.swap(0, Ordering::AcqRel);
                if id == 0 {
                    return Vec::new();
                }
                let id = format!("compaction-{id}");
                let error = metadata.get("error").and_then(serde_json::Value::as_str);
                let status = if metadata
                    .get("cancelled")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                {
                    CompactionStatus::Cancelled
                } else if error.is_some() {
                    CompactionStatus::Failed
                } else {
                    CompactionStatus::Completed
                };
                let mut updates = Vec::new();
                if error.is_none()
                    && let Some(summary) =
                        metadata.get("summary").and_then(serde_json::Value::as_str)
                {
                    updates.push(SessionUpdate::CompactionSummaryChunk(
                        CompactionSummaryChunk::new(
                            id.clone(),
                            ContentBlock::from(summary.to_owned()),
                        ),
                    ));
                }
                let mut completed = CompactionUpdate::new(id, status);
                if let Some(error) = error {
                    completed = completed.error(error.to_owned());
                }
                updates.push(SessionUpdate::CompactionUpdate(completed));
                updates
            }
            _ => Vec::new(),
        }
    }
}
