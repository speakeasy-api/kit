use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use agentkit_core::{
    CancellationController, MetadataMap, SessionId, ToolCallId, ToolOutput, TurnCancellation,
    TurnId,
};
use agentkit_tools_core::{
    AllowAllPermissions, BasicToolExecutor, ToolExecutionOutcome, ToolExecutionScope, ToolName,
    ToolRequest, ToolSource,
};
use serde_json::json;

#[test]
fn runtime_is_rooted_and_exposes_only_compose() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    assert_eq!(runtime.root(), directory.path().canonicalize().unwrap());

    let visible = runtime.compose(0);
    let specs = visible.specs();
    let names = specs
        .iter()
        .map(|spec| spec.name.0.to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["compose"]);

    for child in [
        "docs",
        "shell",
        "edit",
        "subagent",
        "prompt",
        "fork",
        "subagents",
        "close",
        "a2a",
        "tool_search",
        "auth",
        "tool",
    ] {
        assert!(
            visible.get(&ToolName::new(child)).is_some(),
            "missing compose child {child:?}"
        );
    }
}

#[test]
fn compose_advertises_only_configured_acp_harnesses() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "review-agent".into(),
            args: vec!["acp".into()],
            permissions: kit::AcpPermissionPolicy::Deny,
        },
    )]))
    .unwrap();
    let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.kit".into()).unwrap();
    let description = &runtime.compose(0).specs()[0].description;
    assert!(description.contains("\"enum\":[\"acp.kit\",\"acp.review\"]"));
    assert!(!description.contains("acp.unknown"));
}

#[tokio::test]
async fn mcp_meta_tools_are_available_without_an_implicit_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"found = tool_search({ query: "anything" })
return { found }"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("search failed: {outcome:?}")
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({"found": {"servers": []}}))
    );

    let outcome = execute_compose(&runtime, r#"return tool({ name: "shell", args: {} })"#).await;
    assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
}

#[tokio::test]
async fn subagent_selects_an_advertised_acp_model() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture.clone(), "--models".into()],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let runtime =
        kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.review".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"child = subagent({ harness: "acp.review", model: "mock/requested", prompt: "MOCK_SELECTED_MODEL" })
branch = fork({ subagent: child, prompt: "MOCK_SELECTED_MODEL" })
return { child: child.output, branch: branch.output }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("model-selecting subagent failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({
            "child": "mock/requested",
            "branch": "mock/requested"
        }))
    );

    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let runtime = kit::Runtime::with_acp_harnesses(
        kit::Runtime::new(directory.path(), "gpt-5.4").unwrap(),
        harnesses,
        "acp.review".into(),
    )
    .unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"return subagent({ model: "mock/requested", prompt: "never runs" })"#,
    )
    .await;
    let ToolExecutionOutcome::Failed(error) = outcome else {
        panic!("unsupported model selection unexpectedly succeeded: {outcome:?}");
    };
    assert!(format!("{error:?}").contains("does not advertise a selectable model session option"));
}

#[tokio::test]
async fn structured_subagent_output_can_drive_runlet_control_flow() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture],
            permissions: kit::AcpPermissionPolicy::Deny,
        },
    )]))
    .unwrap();
    let runtime =
        kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.review".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"review = subagent({
  harness: "acp.review",
  prompt: "MOCK_STRUCTURED_OUTPUT",
  output_schema: {
    type: "object",
    properties: {
      approved: { type: "boolean" },
      reason: { type: "string" }
    },
    required: ["approved", "reason"],
    additionalProperties: false
  }
})
return if review.output.approved {
  return { reason: review.output.reason }
} else {
  return fail("REJECTED", review.output.reason)
}"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("structured compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({"reason": "mock approved"}))
    );
}

#[tokio::test]
async fn compose_lists_and_closes_subagents_by_handle_or_id() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "review".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let runtime =
        kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.review".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"first = subagent({ prompt: "first" })
second = subagent({ prompt: "second" })
before = after [first, second] { return subagents({}) }
closed_first = after before { return close(first) }
closed_second = after closed_first { return close({ id: second.id }) }
after_close = after closed_second { return subagents({}) }
return { before, closed_first, closed_second, after_close }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("subagent lifecycle compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(value) = result.result.output else {
        panic!("compose did not return structured output");
    };
    assert_eq!(value["before"].as_array().unwrap().len(), 2);
    assert!(value["after_close"].as_array().unwrap().is_empty());
    let before_ids = value["before"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| child["id"].clone())
        .collect::<Vec<_>>();
    assert!(before_ids.contains(&value["closed_first"]["id"]));
    assert!(before_ids.contains(&value["closed_second"]["id"]));
    assert_ne!(value["closed_first"]["id"], value["closed_second"]["id"]);
}

#[tokio::test]
async fn subagent_exposes_bounded_rich_updates_without_changing_text_output() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = kit::AcpHarnesses::new(BTreeMap::from([(
        "rich".into(),
        kit::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let runtime = kit::Runtime::with_acp_harnesses(runtime, harnesses, "acp.rich".into()).unwrap();

    let outcome = execute_compose(
        &runtime,
        r#"child = subagent({ harness: "acp.rich", prompt: "MOCK_RICH_OUTPUT" })
return { output: child.output, updates: child.updates }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("rich compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({
            "output": "rich done",
            "updates": {
                "items": [
                    {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {
                            "type": "image",
                            "data": "aGVsbG8=",
                            "mimeType": "image/png"
                        }
                    },
                    {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "call-1",
                        "title": "Inspect files"
                    },
                    {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "call-1",
                        "status": "completed"
                    },
                    {
                        "sessionUpdate": "plan",
                        "entries": [
                            {"content": "Inspect", "priority": "high", "status": "completed"}
                        ]
                    }
                ],
                "truncated": false
            }
        }))
    );
}

#[tokio::test]
async fn compose_dispatches_bundled_docs_search_through_runlet() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"return docs({ query: "Kit documentation troubleshooting" })"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("compose returned a non-JSON result");
    };
    assert_eq!(output["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        output["matches"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(output["matches"].as_array().unwrap().len() <= 5);
}

#[tokio::test]
async fn compose_dispatches_hidden_shell_through_runlet() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"result = shell({ command: "printf hidden-ok" })
return { code: result.exit_code, ok: result.success, stdout: result.stdout, stderr: result.stderr }"#,
    )
    .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("compose returned a non-JSON result");
    };
    assert_eq!(
        output,
        json!({
            "code": 0,
            "ok": true,
            "stdout": "hidden-ok",
            "stderr": ""
        })
    );
}

#[tokio::test]
async fn independent_top_level_tool_calls_run_concurrently() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"first = shell({ command: "touch first; for i in $(seq 1 100); do test -e second && exit 0; sleep 0.02; done; exit 1" })
second = shell({ command: "touch second; for i in $(seq 1 100); do test -e first && exit 0; sleep 0.02; done; exit 1" })
return { first: first.success, second: second.success }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("concurrent compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({"first": true, "second": true}))
    );
}

#[tokio::test]
async fn after_orders_calls_without_data_flow() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(
        &runtime,
        r#"prerequisite = shell({ command: "sleep 0.1; touch ready" })
dependent = after prerequisite {
  return shell({ command: "test -e ready" })
}
return { prerequisite: prerequisite.success, dependent: dependent.success }"#,
    )
    .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("ordered compose failed: {outcome:?}");
    };
    assert_eq!(
        result.result.output,
        ToolOutput::structured(json!({"prerequisite": true, "dependent": true}))
    );
}

#[tokio::test]
async fn compose_rejects_invalid_hidden_tool_input() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let outcome = execute_compose(&runtime, "return shell({ timeout_seconds: 1 })").await;

    assert!(
        matches!(
            outcome,
            ToolExecutionOutcome::FailedBeforeInvocation(_) | ToolExecutionOutcome::Failed(_)
        ),
        "invalid input unexpectedly ran: {outcome:?}"
    );
}

#[tokio::test]
async fn cancelling_a_turn_stops_a_running_shell_command() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
    let controller = CancellationController::new();
    let cancellation = TurnCancellation::new(controller.handle());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        controller.interrupt();
    });

    let started = Instant::now();
    let outcome = execute_compose_cancelled(
        &runtime,
        r#"return shell({ command: "sleep 30", timeout_seconds: 600 })"#,
        Some(cancellation),
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancellation waited for the command: {outcome:?}"
    );
    assert!(
        matches!(outcome, ToolExecutionOutcome::Failed(_)),
        "cancelled shell did not fail the compose run: {outcome:?}"
    );
}

async fn execute_compose(runtime: &Arc<kit::Runtime>, script: &str) -> ToolExecutionOutcome {
    execute_compose_cancelled(runtime, script, None).await
}

async fn execute_compose_cancelled(
    runtime: &Arc<kit::Runtime>,
    script: &str,
    cancellation: Option<TurnCancellation>,
) -> ToolExecutionOutcome {
    let source: Arc<dyn ToolSource> = Arc::new(runtime.compose(0));
    let executor = Arc::new(BasicToolExecutor::new([source]));
    let scope = ToolExecutionScope {
        executor,
        session_id: SessionId::new("test"),
        turn_id: TurnId::new("turn"),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation,
    };
    scope
        .execute_child(ToolRequest {
            call_id: ToolCallId::new("compose-test"),
            tool_name: ToolName::new("compose"),
            input: json!({"script": script}),
            session_id: SessionId::new("test"),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
        })
        .await
}
