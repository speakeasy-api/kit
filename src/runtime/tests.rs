use std::{sync::Arc, time::Duration};

use agentkit_core::{ItemKind, MetadataMap, Part, SessionId, ToolCallId, ToolOutput, TurnId};
use agentkit_task_manager::RoutingDecision;
use agentkit_tools_core::{
    AllowAllPermissions, BasicToolExecutor, OwnedToolContext, Tool, ToolExecutionOutcome,
    ToolExecutionScope, ToolExecutor, ToolName, ToolRequest, ToolSource,
};
use serde_json::{Value, json};

use super::{
    BackgroundableCompose, Runtime, SessionRequest, SessionSelection, background_route,
    load_initial_transcript,
};

#[test]
fn configured_session_is_consumed_only_after_successful_start() {
    let request = SessionRequest {
        id: "selected".into(),
        resume: false,
        force: false,
    };
    let mut selection = SessionSelection {
        configured: Some(request),
        claimed: false,
    };

    let (first, configured) = selection.claim();
    assert_eq!(first.id, "selected");
    assert!(configured);
    selection.finish(configured, false, true);
    let (retry, configured) = selection.claim();
    assert_eq!(retry.id, "selected");
    assert!(
        retry.resume,
        "a transcript opened before failure is resumed"
    );
    selection.finish(configured, true, false);
    assert!(selection.configured.is_none());
}

#[tokio::test]
async fn loads_all_agents_md_files_outermost_first() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(parent.path().join("AGENTS.md"), "outer guidance").unwrap();
    std::fs::write(root.join("AGENTS.md"), "inner guidance").unwrap();

    let transcript = load_initial_transcript(&root, "system".into())
        .await
        .unwrap();

    assert_eq!(
        transcript.iter().map(|item| item.kind).collect::<Vec<_>>(),
        [ItemKind::System, ItemKind::Context, ItemKind::Context]
    );
    let text = transcript[1..]
        .iter()
        .map(|item| match &item.parts[0] {
            Part::Text(text) => text.text.as_str(),
            other => panic!("expected text context, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(text[0].contains("outer guidance"));
    assert!(text[1].contains("inner guidance"));
}

#[test]
fn compose_is_the_only_visible_tool_and_documents_mcp_meta_tools() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let specs = runtime.compose(0).specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name.0, "compose");
    assert_eq!(
        specs[0].input_schema["properties"]["background"]["oneOf"],
        json!([
            {"type": "boolean"},
            {"type": "integer", "minimum": 1, "maximum": 86_400}
        ])
    );
    assert!(specs[0].description.contains("`tool_search`"));
    assert!(specs[0].description.contains("`auth`"));
    assert!(specs[0].description.contains("`tool`"));
    assert!(!specs[0].description.contains("mcp_filesystem_read_file"));
}

#[test]
fn compose_background_values_select_agentkit_task_routes() {
    let request = |background| {
        ToolRequest::new(
            ToolCallId::new("call"),
            ToolName::new("compose"),
            json!({"script": "return 1", "background": background}),
            SessionId::new("session"),
            TurnId::new("turn"),
        )
    };

    assert_eq!(
        background_route(&request(json!(true))),
        RoutingDecision::ForegroundThenDetachAfter(Duration::ZERO)
    );
    assert_eq!(
        background_route(&request(json!(60))),
        RoutingDecision::ForegroundThenDetachAfter(Duration::from_secs(60))
    );
    for value in [json!(false), json!(0), json!(-1), json!(1.5), json!("60")] {
        assert_eq!(
            background_route(&request(value)),
            RoutingDecision::Foreground
        );
    }
}

#[tokio::test]
async fn compose_background_sanitization_rejects_invalid_and_strips_before_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let request = |background| {
        ToolRequest::new(
            ToolCallId::new("call"),
            ToolName::new("compose"),
            json!({"script": "return 7", "background": background}),
            SessionId::new("session"),
            TurnId::new("turn"),
        )
    };

    async fn invoke(runtime: &Arc<Runtime>, background: Value) -> ToolExecutionOutcome {
        let compose = runtime.compose(0);
        let executor: Arc<dyn ToolExecutor> =
            Arc::new(BasicToolExecutor::new(Vec::<Arc<dyn ToolSource>>::new()));
        let permissions = Arc::new(AllowAllPermissions);
        let resources: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
        let session_id = SessionId::new("session");
        let turn_id = TurnId::new("turn");
        let owned = OwnedToolContext {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            metadata: MetadataMap::new(),
            permissions: permissions.clone(),
            resources: resources.clone(),
            cancellation: None,
            execution_scope: Some(ToolExecutionScope {
                executor,
                session_id,
                turn_id,
                permissions,
                resources,
                cancellation: None,
            }),
            approved_request: None,
        };
        compose
            .backgroundable
            .invoke_outcome(
                ToolRequest::new(
                    ToolCallId::new("call"),
                    ToolName::new("compose"),
                    json!({"script": "return 7", "background": background}),
                    SessionId::new("session"),
                    TurnId::new("turn"),
                ),
                &mut owned.borrowed(),
            )
            .await
    }

    for value in [json!(true), json!(false), json!(1), json!(86_400)] {
        let sanitized = BackgroundableCompose::sanitized(request(value.clone())).unwrap();
        assert!(sanitized.input.get("background").is_none());

        let outcome = invoke(&runtime, value).await;
        let ToolExecutionOutcome::Completed(result) = outcome else {
            panic!("valid background value did not reach compose: {outcome:?}");
        };
        assert_eq!(result.result.output, ToolOutput::structured(json!(7)));
    }

    for value in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(null),
        json!(86_401),
    ] {
        assert!(BackgroundableCompose::sanitized(request(value.clone())).is_err());
        assert!(matches!(
            invoke(&runtime, value).await,
            ToolExecutionOutcome::Failed(_)
        ));
    }
}

#[test]
fn system_prompt_guides_compose_and_subagent_hygiene() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let prompt = runtime.system_prompt(0);
    assert!(prompt.contains("Use compose as a dependency graph"));
    assert!(prompt.contains("use `fold` only for reductions or genuinely sequential chains"));
    assert!(prompt.contains("keeping the session responsive or doing other independent work"));
    assert!(prompt.contains("it also suits one-shot triggers"));
    assert!(prompt.contains("first complete one context-loading subagent, then fork it"));
    assert!(prompt.contains("start fresh subagents from concise summaries"));
    assert!(prompt.contains("close subagents when no longer needed"));
}
