//! Parent-owned durable child identities, separate from model transcript items.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DurableChild {
    pub(crate) id: String,
    pub(crate) acp_session_id: String,
    pub(crate) name: String,
    pub(crate) task: String,
    pub(crate) generation: u64,
    pub(crate) handle_generation: u64,
    pub(crate) output: Value,
    pub(crate) updates: Option<Value>,
    pub(crate) harness: String,
    pub(crate) model: Option<String>,
    pub(crate) root: PathBuf,
    pub(crate) depth: usize,
    pub(crate) lifecycle: ChildLifecycle,
    pub(crate) created_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ChildLifecycle {
    Idle,
    Interrupted,
    Closed,
}

impl DurableChild {
    pub(super) fn validate(&self) -> Result<(), String> {
        super::validate_id(&self.id)?;
        if self.acp_session_id.is_empty()
            || self.harness.is_empty()
            || self.name.is_empty()
            || !self.root.is_absolute()
            || self.depth == 0
            || self.generation == 0
            || self.handle_generation == 0
            || self.handle_generation > self.generation
            || (self.lifecycle == ChildLifecycle::Idle && self.handle_generation != self.generation)
        {
            return Err("invalid durable child identity, workspace, depth, or generation".into());
        }
        Ok(())
    }
}
