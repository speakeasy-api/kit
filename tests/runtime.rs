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
    let names = visible
        .specs()
        .into_iter()
        .map(|spec| spec.name.0.to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["compose"]);
    assert!(visible.specs()[0].description.contains("shell"));
    assert!(visible.get(&ToolName::new("shell")).is_some());
    assert!(visible.get(&ToolName::new("edit")).is_some());
}

#[tokio::test]
async fn compose_dispatches_hidden_shell_through_runlet() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = kit::Runtime::new(directory.path(), "gpt-5.4").unwrap();
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
    let outcome = scope
        .execute_child(ToolRequest {
            call_id: ToolCallId::new("compose-test"),
            tool_name: ToolName::new("compose"),
            input: json!({"script": "return shell({ command: \"printf hidden-ok\" })"}),
            session_id: SessionId::new("test"),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
        })
        .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("compose failed: {outcome:?}");
    };
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("compose returned a non-JSON result");
    };
    assert!(output.to_string().contains("hidden-ok"));
}
