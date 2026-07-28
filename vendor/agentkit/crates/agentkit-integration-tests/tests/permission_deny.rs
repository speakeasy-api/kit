//! Hard-deny permission path: `PermissionDecision::Deny(PermissionDenial)`
//! short-circuits the executor without running the tool body and emits a
//! synthetic error `ToolResult` for the model. Mirrors `approval_flow.rs`
//! but exercises the non-approval branch.

use std::any::Any;

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallId, ToolCallPart,
    ToolOutput, ToolResultPart,
};
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, TurnRecord, assert_recording, snapshot_path,
};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, ModelTurnEvent, ModelTurnResult, SessionConfig,
};
use agentkit_tools_core::{
    PermissionChecker, PermissionCode, PermissionDecision, PermissionDenial, PermissionRequest,
    Tool, ToolContext, ToolError, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::json;

const DENY_KIND: &str = "test.always_deny";

struct DenyRequest {
    metadata: MetadataMap,
}

impl PermissionRequest for DenyRequest {
    fn kind(&self) -> &'static str {
        DENY_KIND
    }
    fn summary(&self) -> String {
        "always-denied op".into()
    }
    fn metadata(&self) -> &MetadataMap {
        &self.metadata
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct UnreachableTool {
    spec: ToolSpec,
}

impl UnreachableTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "denied",
                "Tool body should never execute under the deny checker.",
                json!({ "type": "object", "additionalProperties": true }),
            ),
        }
    }
}

#[async_trait]
impl Tool for UnreachableTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        _request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        Ok(vec![Box::new(DenyRequest {
            metadata: MetadataMap::new(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // The deny checker should short-circuit before this body runs;
        // if it does fire, surface a distinct payload so the snapshot
        // diff makes the regression obvious.
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::text("UNREACHABLE: tool body executed despite Deny"),
        )))
    }
}

struct DenyAlways;

impl PermissionChecker for DenyAlways {
    fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision {
        if request.kind() == DENY_KIND {
            PermissionDecision::Deny(PermissionDenial {
                code: PermissionCode::CustomPolicyDenied,
                message: "policy says no".into(),
                metadata: MetadataMap::new(),
            })
        } else {
            PermissionDecision::Allow
        }
    }
}

#[tokio::test]
async fn permission_deny_short_circuits_and_emits_synthetic_error() {
    let path = snapshot_path("permission_deny.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "deny".into(),
        initial_items: vec![Item::text(ItemKind::User, "do the denied op")],
        turns: vec![
            TurnRecord {
                input: Vec::new(),
                tools: Vec::new(),
                events: vec![
                    ModelTurnEvent::ToolCall(ToolCallPart::new(
                        ToolCallId::new("call-deny"),
                        "denied",
                        json!({}),
                    )),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::ToolCall,
                        output_items: vec![Item::new(
                            ItemKind::Assistant,
                            vec![Part::ToolCall(ToolCallPart::new(
                                ToolCallId::new("call-deny"),
                                "denied",
                                json!({}),
                            ))],
                        )],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ],
            },
            TurnRecord {
                input: Vec::new(),
                tools: Vec::new(),
                events: vec![ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item::new(
                        ItemKind::Assistant,
                        vec![Part::Text(TextPart::new("got blocked, moving on"))],
                    )],
                    usage: None,
                    metadata: MetadataMap::new(),
                })],
            },
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(UnreachableTool::new()))
        .permissions(DenyAlways)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => break,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                panic!("unexpected AwaitingInput before Finished")
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                panic!("Deny must not surface as ApprovalRequest")
            }
        }
    }

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
