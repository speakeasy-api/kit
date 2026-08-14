use std::sync::Arc;

use agentkit_core::{MetadataMap, SessionId, ToolCallId, ToolOutput, TurnId};
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

    let description = &specs[0].description;
    for expected in [
        "`shell`: Run a shell command",
        "`edit`: Apply exact, git-style text hunks",
        "`subagent`: Run a fresh local coding agent",
        "`a2a`: Send a text task",
    ] {
        assert!(description.contains(expected), "missing {expected:?}");
    }
    assert_eq!(description.matches("Input JSON schema:").count(), 4);
    assert_eq!(description.matches("Output JSON schema:").count(), 4);
    assert!(description.contains("\"exit_code\""));
    assert!(description.contains("\"enum\":[\"delete\"]"));
    assert!(!description.contains("\"const\""));

    assert!(visible.get(&ToolName::new("shell")).is_some());
    assert!(visible.get(&ToolName::new("edit")).is_some());
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

async fn execute_compose(runtime: &Arc<kit::Runtime>, script: &str) -> ToolExecutionOutcome {
    let source: Arc<dyn ToolSource> = Arc::new(runtime.compose(0));
    let executor = Arc::new(BasicToolExecutor::new([source]));
    let scope = ToolExecutionScope {
        executor,
        session_id: SessionId::new("test"),
        turn_id: TurnId::new("turn"),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
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
