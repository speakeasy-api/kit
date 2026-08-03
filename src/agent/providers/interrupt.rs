use std::fmt;

use agentkit_loop::{AgentBuilder, LoopInterrupt, LoopStep, ModelAdapter, ModelSession};

use crate::store::sqlite::append::SqliteStore;
use crate::{
    agent::driver::{
        attempt::{AttemptDriver, PollError},
        restart::{BoundarySnapshot, LoopRecord, RestartPlan, SafeBoundary, StartError},
        waiting::{WaitingKind, WaitingState},
    },
    api::service::{AttemptDriverClaim, AttemptProjection},
    domain::ids::{ApprovalId, CommandId, RunId, ToolCallId},
};

#[derive(Clone, Debug, PartialEq)]
pub enum InterruptBoundary {
    Waiting(Box<WaitingState>),
    Cooperative,
    Finished(Box<agentkit_loop::TurnResult>),
}

#[derive(Debug)]
pub enum InterruptError {
    Poll(PollError),
    InvalidBoundary(&'static str),
    Identifier,
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poll(error) => error.fmt(f),
            Self::InvalidBoundary(message) => f.write_str(message),
            Self::Identifier => f.write_str("could not allocate durable interruption identifier"),
        }
    }
}

impl std::error::Error for InterruptError {}

pub async fn poll_attempt<S>(
    driver: &mut AttemptDriver<S>,
    projection: &AttemptProjection,
    snapshot: BoundarySnapshot,
) -> Result<InterruptBoundary, InterruptError>
where
    S: ModelSession,
{
    match driver
        .poll(projection)
        .await
        .map_err(InterruptError::Poll)?
    {
        LoopStep::Finished(result) => Ok(InterruptBoundary::Finished(Box::new(result))),
        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
            Ok(InterruptBoundary::Cooperative)
        }
        LoopStep::Interrupt(interrupt) => waiting_from_loop(interrupt, projection, snapshot)
            .map(Box::new)
            .map(InterruptBoundary::Waiting),
    }
}

pub fn waiting_record(
    interrupt: LoopInterrupt,
    projection: &AttemptProjection,
    snapshot: BoundarySnapshot,
) -> Result<LoopRecord, InterruptError> {
    waiting_from_loop(interrupt, projection, snapshot).map(LoopRecord::Waiting)
}

pub fn auth_waiting_record(
    projection: &AttemptProjection,
    run_id: RunId,
    scope: impl Into<String>,
    snapshot: BoundarySnapshot,
) -> Result<LoopRecord, InterruptError> {
    auth_waiting_record_inner(projection, run_id, scope.into(), snapshot, None)
}

pub fn challenge_auth_waiting_record(
    projection: &AttemptProjection,
    run_id: RunId,
    challenge: &crate::capabilities::broker::AuthChallenge,
    snapshot: BoundarySnapshot,
) -> Result<LoopRecord, InterruptError> {
    auth_waiting_record_inner(
        projection,
        run_id,
        challenge.scope.clone(),
        snapshot,
        Some(challenge),
    )
}

fn auth_waiting_record_inner(
    projection: &AttemptProjection,
    run_id: RunId,
    scope: String,
    snapshot: BoundarySnapshot,
    challenge: Option<&crate::capabilities::broker::AuthChallenge>,
) -> Result<LoopRecord, InterruptError> {
    if run_id != projection.run_id {
        return Err(InterruptError::InvalidBoundary(
            "provider auth run does not match the attempt",
        ));
    }
    if snapshot.boundary != SafeBoundary::BeforeModelDispatch {
        return Err(InterruptError::InvalidBoundary(
            "provider auth can only pause before model dispatch",
        ));
    }
    Ok(LoopRecord::Waiting(WaitingState {
        wait_id: CommandId::generate().map_err(|_| InterruptError::Identifier)?,
        principal_id: projection.owner.principal_id,
        kind: WaitingKind::Auth {
            run_id,
            scope,
            tool_call_id: None,
            challenge_kind: challenge.map_or(
                crate::agent::driver::waiting::AuthChallengeKind::Provider,
                |challenge| match challenge.kind {
                    crate::capabilities::broker::AuthChallengeKind::Broker => {
                        crate::agent::driver::waiting::AuthChallengeKind::Broker
                    }
                    crate::capabilities::broker::AuthChallengeKind::Transport => {
                        crate::agent::driver::waiting::AuthChallengeKind::Transport
                    }
                },
            ),
            challenge_generation: challenge.map_or(0, |challenge| challenge.generation),
            challenge_id: challenge.map(|challenge| challenge.challenge_id),
        },
        snapshot,
    }))
}

pub async fn restart_resolved<M, F>(
    plan: RestartPlan,
    projection: &AttemptProjection,
    claim: AttemptDriverClaim,
    store: SqliteStore,
    model: M,
    configure: F,
) -> Result<AttemptDriver<crate::agent::driver::restart::ReplaySession<M::Session>>, StartError>
where
    M: ModelAdapter,
    F: FnOnce(
        AgentBuilder<crate::agent::driver::restart::ReplayAdapter<M>>,
    ) -> AgentBuilder<crate::agent::driver::restart::ReplayAdapter<M>>,
{
    plan.start_claimed(projection, claim, store, model, configure)
        .await
}

fn waiting_from_loop(
    interrupt: LoopInterrupt,
    projection: &AttemptProjection,
    snapshot: BoundarySnapshot,
) -> Result<WaitingState, InterruptError> {
    let kind = match interrupt {
        LoopInterrupt::AwaitingInput(_) => {
            if snapshot.boundary != SafeBoundary::TurnEnd {
                return Err(InterruptError::InvalidBoundary(
                    "input interruption requires a passive turn-end boundary",
                ));
            }
            WaitingKind::Input
        }
        LoopInterrupt::ApprovalRequest(pending) => {
            if pending.request.request_kind == "kit.mcp.auth" {
                if snapshot.boundary != SafeBoundary::AfterModelOutcome {
                    return Err(InterruptError::InvalidBoundary(
                        "MCP auth interruption requires a committed model outcome",
                    ));
                }
                let tool_call_id = pending
                    .request
                    .call_id
                    .as_ref()
                    .and_then(|id| ToolCallId::parse(&id.0).ok())
                    .ok_or(InterruptError::InvalidBoundary(
                        "MCP auth interruption has no valid tool call",
                    ))?;
                let scope = pending
                    .request
                    .metadata
                    .get("kit.mcp.auth_scope")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(InterruptError::InvalidBoundary(
                        "MCP auth interruption has no scope",
                    ))?;
                let challenge_kind = pending
                    .request
                    .metadata
                    .get("kit.mcp.auth_challenge_kind")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|kind| match kind {
                        "broker" => Some(crate::agent::driver::waiting::AuthChallengeKind::Broker),
                        "transport" => {
                            Some(crate::agent::driver::waiting::AuthChallengeKind::Transport)
                        }
                        _ => None,
                    })
                    .ok_or(InterruptError::InvalidBoundary(
                        "MCP auth interruption has no challenge kind",
                    ))?;
                let challenge_generation = pending
                    .request
                    .metadata
                    .get("kit.mcp.auth_challenge_generation")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or(InterruptError::InvalidBoundary(
                        "MCP auth interruption has no challenge generation",
                    ))?;
                let challenge_id = pending
                    .request
                    .metadata
                    .get("kit.mcp.auth_challenge_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| ApprovalId::parse(value).ok())
                    .ok_or(InterruptError::InvalidBoundary(
                        "MCP auth interruption has no valid challenge ID",
                    ))?;
                return Ok(WaitingState {
                    wait_id: CommandId::generate().map_err(|_| InterruptError::Identifier)?,
                    principal_id: projection.owner.principal_id,
                    kind: WaitingKind::Auth {
                        run_id: projection.run_id,
                        scope: scope.to_owned(),
                        tool_call_id: Some(tool_call_id),
                        challenge_kind,
                        challenge_generation,
                        challenge_id: Some(challenge_id),
                    },
                    snapshot,
                });
            }
            if snapshot.boundary != SafeBoundary::AfterModelOutcome {
                return Err(InterruptError::InvalidBoundary(
                    "approval interruption requires a committed model outcome",
                ));
            }
            WaitingKind::Approval {
                approval_id: match ApprovalId::parse(&pending.request.id.0) {
                    Ok(id) => id,
                    Err(_) => ApprovalId::generate().map_err(|_| InterruptError::Identifier)?,
                },
                tool_call_id: pending
                    .request
                    .call_id
                    .as_ref()
                    .and_then(|id| ToolCallId::parse(&id.0).ok())
                    .unwrap_or(ToolCallId::generate().map_err(|_| InterruptError::Identifier)?),
            }
        }
        LoopInterrupt::AfterToolResult(_) => {
            return Err(InterruptError::InvalidBoundary(
                "cooperative tool boundaries are not durable waiting states",
            ));
        }
    };
    Ok(WaitingState {
        wait_id: CommandId::generate().map_err(|_| InterruptError::Identifier)?,
        principal_id: projection.owner.principal_id,
        kind,
        snapshot,
    })
}
