use agentkit_core::{
    ApprovalId, FinishReason, Item, ItemKind, MetadataMap, Part, ToolCallId, ToolCallPart,
    ToolOutput,
};
use agentkit_integration_tests::mock_model::{MockAdapter, TurnScript};
use agentkit_integration_tests::mock_tool::RecordingTool;
use agentkit_loop::{Agent, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_tools_core::{
    ApprovalReason, ApprovalRequest, PermissionChecker, PermissionDecision, PermissionRequest,
    Tool, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSource, ToolSpec,
    dynamic_catalog,
};
use async_trait::async_trait;
use serde_json::json;
use std::any::Any;
use std::sync::Arc;

fn compose_call(script: &str) -> ToolCallPart {
    ToolCallPart::new(
        ToolCallId::new("compose-call"),
        "compose",
        json!({ "script": script }),
    )
}

async fn start(
    agent: Agent<MockAdapter>,
) -> agentkit_loop::LoopDriver<agentkit_integration_tests::mock_model::MockSession> {
    agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new("compose-session"),
        vec![Item::text(ItemKind::User, "compose tools")],
    )
    .await
}

#[tokio::test]
async fn compose_script_calls_child_tools_and_lands_one_result() {
    let adapter = MockAdapter::new();
    adapter.enqueue_many([
        TurnScript::tool_call(compose_call(
            "local a = tool('echo', { value = 1 }); local b = tool('echo', { value = a.value + 1 }); return b",
        )),
        TurnScript::text("done"),
    ]);

    let echo = RecordingTool::new(
        ToolSpec::new("echo", "echo input", json!({"type": "object"})),
        |request| Ok(ToolOutput::structured(request.input.clone())),
    );
    let tools = agentkit_tool_compose::registry().with(echo.clone());
    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .build()
        .unwrap();
    let mut driver = start(agent).await;

    match driver.next().await.unwrap() {
        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {}
        other => panic!("expected AfterToolResult, got {other:?}"),
    }
    match driver.next().await.unwrap() {
        LoopStep::Finished(result) => assert_eq!(result.finish_reason, FinishReason::Completed),
        other => panic!("expected finished turn, got {other:?}"),
    }

    assert_eq!(echo.call_count(), 2);
    let transcript = driver.snapshot().transcript;
    let compose_results: Vec<_> = transcript
        .iter()
        .flat_map(|item| item.parts.iter())
        .filter_map(|part| match part {
            Part::ToolResult(result) if result.call_id == ToolCallId::new("compose-call") => {
                Some(result)
            }
            _ => None,
        })
        .collect();
    assert_eq!(compose_results.len(), 1);
    assert_eq!(
        compose_results[0].output,
        ToolOutput::structured(json!({ "value": 2 }))
    );
}

struct ApprovalPermissionRequest {
    metadata: MetadataMap,
}

impl PermissionRequest for ApprovalPermissionRequest {
    fn kind(&self) -> &'static str {
        "compose.integration.approval"
    }

    fn summary(&self) -> String {
        "approval required".into()
    }

    fn metadata(&self) -> &MetadataMap {
        &self.metadata
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
struct ApprovalEchoTool {
    spec: ToolSpec,
}

impl ApprovalEchoTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec::new("approval_echo", "approval echo", json!({"type": "object"})),
        }
    }
}

#[async_trait]
impl Tool for ApprovalEchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn proposed_requests(
        &self,
        _request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        Ok(vec![Box::new(ApprovalPermissionRequest {
            metadata: MetadataMap::new(),
        })])
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::new(agentkit_core::ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(request.input),
        )))
    }
}

struct RequireApproval;

impl PermissionChecker for RequireApproval {
    fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision {
        PermissionDecision::RequireApproval(ApprovalRequest {
            task_id: None,
            call_id: None,
            id: ApprovalId::new("approval:compose-child"),
            request_kind: request.kind().into(),
            reason: ApprovalReason::PolicyRequiresConfirmation,
            summary: request.summary(),
            metadata: request.metadata().clone(),
        })
    }
}

#[tokio::test]
async fn compose_nested_approval_surfaces_and_resumes() {
    let adapter = MockAdapter::new();
    adapter.enqueue_many([
        TurnScript::tool_call(compose_call(
            "local out = tool('approval_echo', { value = 9 }); return out",
        )),
        TurnScript::text("approved done"),
    ]);

    let agent = Agent::builder()
        .model(adapter)
        .permissions(RequireApproval)
        .add_tool_source(agentkit_tool_compose::registry().with(ApprovalEchoTool::new()))
        .build()
        .unwrap();
    let mut driver = start(agent).await;

    let pending = match driver.next().await.unwrap() {
        LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => pending,
        other => panic!("expected approval interrupt, got {other:?}"),
    };
    assert_eq!(
        pending.request.id,
        ApprovalId::new("approval:compose-child")
    );
    pending.approve(&mut driver).unwrap();

    match driver.next().await.unwrap() {
        LoopStep::Finished(result) => assert_eq!(result.finish_reason, FinishReason::Completed),
        other => panic!("expected finished turn, got {other:?}"),
    }

    let transcript = driver.snapshot().transcript;
    let result = transcript
        .iter()
        .flat_map(|item| item.parts.iter())
        .find_map(|part| match part {
            Part::ToolResult(result) if result.call_id == ToolCallId::new("compose-call") => {
                Some(result)
            }
            _ => None,
        })
        .expect("compose result");
    assert_eq!(result.output, ToolOutput::structured(json!({ "value": 9 })));
    assert!(!result.is_error);
}

#[tokio::test]
async fn compose_tools_listing_excludes_compose_and_lists_children() {
    let adapter = MockAdapter::new();
    // Build a sorted, unique list of tool names visible to the script and
    // return it. The host test asserts the set is exactly {echo, reverse}.
    let script = r#"
        local seen = {}
        for _, spec in ipairs(tools()) do
            seen[spec.name] = true
        end
        if seen['compose'] then
            error('compose should not be visible to itself by default')
        end
        local names = {}
        for name, _ in pairs(seen) do
            table.insert(names, name)
        end
        table.sort(names)
        return names
    "#;
    adapter.enqueue_many([
        TurnScript::tool_call(compose_call(script)),
        TurnScript::text("done"),
    ]);

    let echo = RecordingTool::new(
        ToolSpec::new("echo", "echo input", json!({"type": "object"})),
        |request| Ok(ToolOutput::structured(request.input.clone())),
    );
    let reverse = RecordingTool::new(
        ToolSpec::new("reverse", "reverse input", json!({"type": "object"})),
        |request| Ok(ToolOutput::structured(request.input.clone())),
    );
    let tools = agentkit_tool_compose::registry().with(echo).with(reverse);
    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .build()
        .unwrap();
    let mut driver = start(agent).await;

    match driver.next().await.unwrap() {
        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {}
        other => panic!("expected AfterToolResult, got {other:?}"),
    }
    match driver.next().await.unwrap() {
        LoopStep::Finished(result) => assert_eq!(result.finish_reason, FinishReason::Completed),
        other => panic!("expected finished turn, got {other:?}"),
    }

    let transcript = driver.snapshot().transcript;
    let compose_result = transcript
        .iter()
        .flat_map(|item| item.parts.iter())
        .find_map(|part| match part {
            Part::ToolResult(result) if result.call_id == ToolCallId::new("compose-call") => {
                Some(result)
            }
            _ => None,
        })
        .expect("compose result");
    assert_eq!(
        compose_result.output,
        ToolOutput::structured(json!(["echo", "reverse"])),
        "tools() should expose every child tool and exclude compose itself",
    );
}

#[tokio::test]
async fn compose_source_tracks_dynamic_child_catalog() {
    let (writer, reader) = dynamic_catalog("dynamic");
    let alpha = RecordingTool::new(
        ToolSpec::new("alpha", "alpha input", json!({"type": "object"})).with_output_schema(
            json!({
                "type": "object",
                "properties": {
                    "value": { "type": "integer" }
                }
            }),
        ),
        |request| Ok(ToolOutput::structured(request.input.clone())),
    );
    writer.upsert(Arc::new(alpha));
    let _ = reader.drain_catalog_events();

    let tools = agentkit_tool_compose::ComposeTool::wrap(reader);

    let specs = tools.specs();
    assert!(specs.iter().any(|spec| spec.name.0 == "compose"));
    assert!(specs.iter().any(|spec| spec.name.0 == "alpha"));
    let compose_spec = specs
        .iter()
        .find(|spec| spec.name.0 == "compose")
        .expect("compose spec");
    assert!(compose_spec.description.contains("alpha"));
    assert!(compose_spec.description.contains("\"value\""));
    assert!(tools.get(&ToolName::new("alpha")).is_some());

    assert!(writer.remove(&ToolName::new("alpha")));

    let events = tools.drain_catalog_events();
    assert!(
        events
            .iter()
            .any(|event| event.removed.iter().any(|name| name == "alpha")),
        "dynamic child removal must be forwarded: {events:?}",
    );
    assert!(
        events
            .iter()
            .any(|event| event.changed.iter().any(|name| name == "compose")),
        "compose spec must be marked changed when child catalog changes: {events:?}",
    );

    let specs = tools.specs();
    assert!(!specs.iter().any(|spec| spec.name.0 == "alpha"));
    let compose_spec = specs
        .iter()
        .find(|spec| spec.name.0 == "compose")
        .expect("compose spec");
    assert!(!compose_spec.description.contains("alpha"));
    assert!(tools.get(&ToolName::new("alpha")).is_none());
}

#[tokio::test]
async fn compose_replays_completed_children_across_resume() {
    // Two non-gated child calls precede one gated call. On first run we
    // expect echo to fire twice and the gated tool to interrupt. After
    // approval, the resumed run must NOT re-execute the two completed echo
    // calls (they replay from records), but it must invoke approval_echo
    // exactly once and surface all three values in the final result.
    let adapter = MockAdapter::new();
    let script = "local a = tool('echo', { v = 1 }); \
                  local b = tool('echo', { v = 2 }); \
                  local c = tool('approval_echo', { v = 3 }); \
                  return { a = a, b = b, c = c }";
    adapter.enqueue_many([
        TurnScript::tool_call(compose_call(script)),
        TurnScript::text("approved done"),
    ]);

    let echo = RecordingTool::new(
        ToolSpec::new("echo", "echo input", json!({"type": "object"})),
        |request| Ok(ToolOutput::structured(request.input.clone())),
    );
    let approval_echo = ApprovalEchoTool::new();
    let tools = agentkit_tool_compose::registry()
        .with(echo.clone())
        .with(approval_echo);
    let agent = Agent::builder()
        .model(adapter)
        .permissions(RequireApproval)
        .add_tool_source(tools)
        .build()
        .unwrap();
    let mut driver = start(agent).await;

    let pending = match driver.next().await.unwrap() {
        LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => pending,
        other => panic!("expected approval interrupt, got {other:?}"),
    };
    assert_eq!(
        pending.request.id,
        ApprovalId::new("approval:compose-child")
    );
    assert_eq!(
        echo.call_count(),
        2,
        "both non-gated children must run before the gated child interrupts"
    );
    pending.approve(&mut driver).unwrap();

    match driver.next().await.unwrap() {
        LoopStep::Finished(result) => assert_eq!(result.finish_reason, FinishReason::Completed),
        other => panic!("expected finished turn, got {other:?}"),
    }

    // The completed echo calls must be replayed via records on resume —
    // their actual invoke must not run a second time.
    assert_eq!(
        echo.call_count(),
        2,
        "completed children must not be re-invoked on resume"
    );

    let transcript = driver.snapshot().transcript;
    let compose_result = transcript
        .iter()
        .flat_map(|item| item.parts.iter())
        .find_map(|part| match part {
            Part::ToolResult(result) if result.call_id == ToolCallId::new("compose-call") => {
                Some(result)
            }
            _ => None,
        })
        .expect("compose result");
    assert_eq!(
        compose_result.output,
        ToolOutput::structured(json!({
            "a": { "v": 1 },
            "b": { "v": 2 },
            "c": { "v": 3 },
        }))
    );
    assert!(!compose_result.is_error);
}
