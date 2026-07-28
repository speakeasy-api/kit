use std::fmt;

use agentkit_core::{
    Item, ItemKind, MetadataMap, Part, ToolCallId as AgentkitToolCallId, ToolOutput, ToolResultPart,
};
use serde::{Deserialize, Serialize};

use crate::{
    agent::{
        agentkit_bridge::mapping::{CanonicalItem, from_agentkit_item},
        driver::restart::{BoundarySnapshot, LoopRecord, SafeBoundary},
    },
    api::auth::contract::AuthenticatedPrincipal,
    domain::{
        events::ApprovalDecision,
        ids::{ApprovalId, CommandId, PrincipalId, RunId, ToolCallId},
    },
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitingKind {
    Input,
    Approval {
        approval_id: ApprovalId,
        tool_call_id: ToolCallId,
    },
    Auth {
        run_id: RunId,
        scope: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WaitingState {
    pub wait_id: CommandId,
    pub principal_id: PrincipalId,
    pub kind: WaitingKind,
    pub snapshot: BoundarySnapshot,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitingResolution {
    Input { items: Vec<CanonicalItem> },
    Approval { decision: ApprovalDecision },
    Auth { granted: bool },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WaitingResolved {
    pub(crate) wait_id: CommandId,
    pub(crate) resolved_by: PrincipalId,
    pub(crate) resolution: WaitingResolution,
    pub(crate) snapshot: BoundarySnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitingError {
    Unauthorized,
    ResolutionMismatch,
    InvalidInputBoundary,
}

impl fmt::Display for WaitingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("waiting state resolution principal does not match"),
            Self::ResolutionMismatch => {
                f.write_str("resolution does not match the durable waiting state")
            }
            Self::InvalidInputBoundary => {
                f.write_str("input can only resolve a passive waiting boundary")
            }
        }
    }
}

impl std::error::Error for WaitingError {}

impl WaitingState {
    pub fn resolve_input(
        &self,
        authenticated: &AuthenticatedPrincipal,
        items: Vec<Item>,
    ) -> Result<LoopRecord, WaitingError> {
        self.resolve(
            authenticated,
            WaitingResolution::Input {
                items: items.iter().map(from_agentkit_item).collect(),
            },
        )
    }

    pub fn resolve(
        &self,
        authenticated: &AuthenticatedPrincipal,
        resolution: WaitingResolution,
    ) -> Result<LoopRecord, WaitingError> {
        if authenticated.principal_id() != self.principal_id {
            return Err(WaitingError::Unauthorized);
        }
        if !matches!(
            (&self.kind, &resolution),
            (WaitingKind::Input, WaitingResolution::Input { .. })
                | (
                    WaitingKind::Approval { .. },
                    WaitingResolution::Approval { .. }
                )
                | (WaitingKind::Auth { .. }, WaitingResolution::Auth { .. })
        ) {
            return Err(WaitingError::ResolutionMismatch);
        }

        let mut snapshot = self.snapshot.clone();
        match (&self.kind, &resolution) {
            (_, WaitingResolution::Input { items }) => {
                if items.is_empty()
                    || snapshot.resume_index.is_some()
                    || snapshot.model_outcome.is_some()
                {
                    return Err(WaitingError::InvalidInputBoundary);
                }
                let resume_index = snapshot.transcript.len();
                snapshot.transcript.extend(items.clone());
                snapshot.boundary = SafeBoundary::BeforeModelDispatch;
                snapshot.resume_index = Some(resume_index);
            }
            (
                WaitingKind::Approval { tool_call_id, .. },
                WaitingResolution::Approval {
                    decision: ApprovalDecision::Denied,
                },
            ) => {
                let resume_index = snapshot.transcript.len();
                snapshot.transcript.push(from_agentkit_item(&Item::new(
                    ItemKind::Tool,
                    vec![Part::ToolResult(ToolResultPart {
                        call_id: AgentkitToolCallId::new(tool_call_id.to_string()),
                        output: ToolOutput::Text("approval denied".into()),
                        is_error: true,
                        metadata: MetadataMap::new(),
                    })],
                )));
                snapshot.boundary = SafeBoundary::AfterToolOutcome;
                snapshot.resume_index = Some(resume_index);
                snapshot.model_outcome = None;
            }
            _ => {}
        }

        Ok(LoopRecord::WaitingResolved(WaitingResolved {
            wait_id: self.wait_id,
            resolved_by: authenticated.principal_id(),
            resolution,
            snapshot,
        }))
    }
}
