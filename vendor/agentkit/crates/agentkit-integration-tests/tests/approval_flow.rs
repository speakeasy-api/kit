//! End-to-end approval interrupt: a tool declares a permission request
//! that the host's [`PermissionChecker`] gates behind approval. The loop
//! emits [`LoopInterrupt::ApprovalRequest`]; the test resolves it via
//! `pending.approve(&mut driver)`, and the tool subsequently executes.
//!
//! Each scenario is captured as a single [`SessionRecording`] in
//! `tests/snapshots/`. The mock model is driven from the recording's
//! scripted events; the recording's transcript / tool catalog / final
//! state are verified after the test drives the loop. Update with
//! `UPDATE_SNAPSHOTS=1`.

use std::any::Any;

use agentkit_core::{MetadataMap, MetadataMap as Meta, ToolOutput, ToolResultPart};
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, assert_recording, snapshot_path,
};
use agentkit_loop::{Agent, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_tools_core::{
    ApprovalReason, ApprovalRequest, PermissionChecker, PermissionDecision, PermissionRequest,
    Tool, ToolContext, ToolError, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::json;

const APPROVAL_KIND: &str = "test.dangerous_op";

struct DangerRequest {
    summary: String,
    metadata: MetadataMap,
}

impl PermissionRequest for DangerRequest {
    fn kind(&self) -> &'static str {
        APPROVAL_KIND
    }
    fn summary(&self) -> String {
        self.summary.clone()
    }
    fn metadata(&self) -> &MetadataMap {
        &self.metadata
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct GatedTool {
    spec: ToolSpec,
}

impl GatedTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "danger",
                "Performs a dangerous op gated by approval.",
                json!({ "type": "object", "additionalProperties": true }),
            ),
        }
    }
}

#[async_trait]
impl Tool for GatedTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        _request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        Ok(vec![Box::new(DangerRequest {
            summary: "execute danger".into(),
            metadata: Meta::new(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::text("danger:executed"),
        )))
    }
}

struct ApproveDangerOnly;

impl PermissionChecker for ApproveDangerOnly {
    fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision {
        if request.kind() == APPROVAL_KIND {
            PermissionDecision::RequireApproval(ApprovalRequest::new(
                "approval:test",
                APPROVAL_KIND,
                ApprovalReason::EscalatedRisk,
                request.summary(),
            ))
        } else {
            PermissionDecision::Allow
        }
    }
}

#[tokio::test]
async fn approval_interrupt_pauses_then_resumes_after_approve() {
    let path = snapshot_path("approval_flow_approve.ron");
    let recording = SessionRecording::load(&path);
    let adapter = SnapshotAdapter::from_recording(&recording);

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(GatedTool::new()))
        .permissions(ApproveDangerOnly)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    let pending = loop {
        match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(p)) => break p,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                panic!("expected approval interrupt before AwaitingInput")
            }
            LoopStep::Finished(_) => panic!("expected approval interrupt before Finished"),
        }
    };
    pending.approve(&mut driver).unwrap();

    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => break,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            other => panic!("unexpected step after approval: {other:?}"),
        }
    }

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

#[tokio::test]
async fn approval_interrupt_denies_to_synthetic_error_result() {
    let path = snapshot_path("approval_flow_deny.ron");
    let recording = SessionRecording::load(&path);
    let adapter = SnapshotAdapter::from_recording(&recording);

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(GatedTool::new()))
        .permissions(ApproveDangerOnly)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    let pending = loop {
        match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(p)) => break p,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            other => panic!("expected approval interrupt, got {other:?}"),
        }
    };
    pending
        .deny_with_reason(&mut driver, "policy says no")
        .unwrap();

    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => break,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            other => panic!("unexpected step after deny: {other:?}"),
        }
    }

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
