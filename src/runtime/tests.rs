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
        configured_claimed: false,
        generated_retries: Default::default(),
    };

    let (first, configured) = selection.claim();
    assert_eq!(first.id, "selected");
    assert!(configured);
    selection.finish(&first, configured, false, true);
    let (retry, configured) = selection.claim();
    assert_eq!(retry.id, "selected");
    assert!(
        retry.resume,
        "a transcript opened before failure is resumed"
    );
    selection.finish(&retry, configured, true, false);
    assert!(selection.configured.is_none());
}

#[test]
fn dropped_configured_session_claim_is_retriable_and_resumes_after_open() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::with_session(
        root.path(),
        "gpt-5.4",
        SessionRequest {
            id: "selected".into(),
            resume: false,
            force: false,
        },
    )
    .unwrap();

    let mut failed = runtime.claim_session().unwrap();
    assert_eq!(failed.id(), "selected");
    failed.mark_opened();
    drop(failed);

    let retry = runtime.claim_session().unwrap();
    assert_eq!(retry.id(), "selected");
    assert!(retry.request.resume);
    retry.commit().unwrap();

    let generated = runtime.claim_session().unwrap();
    assert!(generated.id().starts_with("s-"));
}

#[test]
fn dropped_generated_session_claim_retries_opened_transcript_with_same_id() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();

    let mut failed = runtime.claim_session().unwrap();
    let session_id = failed.id().to_owned();
    let opened = crate::session::open(
        root.path(),
        &session_id,
        false,
        false,
        vec![agentkit_core::Item::text(ItemKind::System, "system")],
    )
    .unwrap();
    failed.mark_opened();
    drop(opened);
    drop(failed);

    let retry = runtime.claim_session().unwrap();
    assert_eq!(retry.id(), session_id);
    assert!(retry.request.resume);
    let reopened = crate::session::open(
        root.path(),
        retry.id(),
        true,
        false,
        vec![agentkit_core::Item::text(ItemKind::System, "system")],
    )
    .unwrap();
    drop(reopened);
    retry.commit().unwrap();

    let next = runtime.claim_session().unwrap();
    assert_ne!(next.id(), session_id);
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

fn write_skill(directory: &std::path::Path, name: &str, description: &str, body: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

#[test]
fn plugin_skills_join_the_catalog_without_broadening_discovery() {
    let root = tempfile::tempdir().unwrap();
    let project_skill = root.path().join(".agents/skills/project-skill");
    write_skill(
        &project_skill,
        "project-skill",
        "Project skill.",
        "project body",
    );

    let plugin = root.path().join("plugin");
    let plugin_skill = plugin.join("skills/plugin-skill");
    write_skill(
        &plugin_skill,
        "plugin-skill",
        "Plugin skill.",
        "plugin body",
    );
    write_skill(
        &plugin_skill.join("nested-skill"),
        "nested-skill",
        "Nested skill.",
        "nested body",
    );

    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let runtime = Runtime::with_plugin_skills(runtime, vec![plugin], vec![plugin_skill]).unwrap();
    let skills = runtime.skills.tool_registry();
    let tool = ToolSource::get(&skills, &ToolName::new("activate_skill")).unwrap();
    let spec = tool.current_spec().unwrap();
    let catalog = spec.input_schema.to_string();
    assert!(catalog.contains("project-skill"));
    assert!(catalog.contains("plugin-skill"));
    assert!(!catalog.contains("nested-skill"));
}

#[cfg(unix)]
#[test]
fn plugin_skill_symlink_retargeting_fails_closed() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let plugin = root.path().join("plugin");
    let skill = plugin.join("skills/plugin-skill");
    write_skill(&skill, "plugin-skill", "Plugin skill.", "safe body");
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let runtime = Runtime::with_plugin_skills(runtime, vec![plugin], vec![skill.clone()]).unwrap();

    let outside = tempfile::tempdir().unwrap();
    let replacement = outside.path().join("SKILL.md");
    std::fs::write(
        &replacement,
        "---\nname: plugin-skill\ndescription: Replaced skill.\n---\noutside body\n",
    )
    .unwrap();
    std::fs::remove_file(skill.join("SKILL.md")).unwrap();
    symlink(replacement, skill.join("SKILL.md")).unwrap();

    let skills = runtime.skills.tool_registry();
    let tool = ToolSource::get(&skills, &ToolName::new("activate_skill")).unwrap();
    let catalog = tool
        .current_spec()
        .map(|spec| spec.description)
        .unwrap_or_default();
    assert!(!catalog.contains("plugin-skill"));
    assert!(!catalog.contains("Replaced skill."));
}

#[test]
fn project_skills_take_precedence_over_plugin_skills() {
    let root = tempfile::tempdir().unwrap();
    write_skill(
        &root.path().join(".agents/skills/shared-skill"),
        "shared-skill",
        "Project version.",
        "project body",
    );
    let plugin_skill = root.path().join("skills/shared-skill");
    write_skill(
        &plugin_skill,
        "shared-skill",
        "Plugin version.",
        "plugin body",
    );

    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let runtime =
        Runtime::with_plugin_skills(runtime, vec![root.path().to_path_buf()], vec![plugin_skill])
            .unwrap();
    let skills = runtime.skills.tool_registry();
    let tool = ToolSource::get(&skills, &ToolName::new("activate_skill")).unwrap();
    let catalog = tool.current_spec().unwrap().description;
    assert!(catalog.contains("Project version."));
    assert!(!catalog.contains("Plugin version."));
}

#[tokio::test]
async fn session_skill_registries_reset_independently() {
    let root = tempfile::tempdir().unwrap();
    write_skill(
        &root.path().join(".agents/skills/reusable"),
        "reusable",
        "Reusable skill.",
        "full instructions",
    );
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let registry = runtime.fresh_skills();
    let other_registry = runtime.fresh_skills();
    let skills = registry.tool_registry();
    let other_skills = other_registry.tool_registry();
    let tool = ToolSource::get(&skills, &ToolName::new("activate_skill")).unwrap();
    let other_tool = ToolSource::get(&other_skills, &ToolName::new("activate_skill")).unwrap();
    let permissions = Arc::new(AllowAllPermissions);
    let resources: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
    let owned = OwnedToolContext {
        session_id: SessionId::new("session"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions,
        resources,
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let request = |call_id| {
        ToolRequest::new(
            ToolCallId::new(call_id),
            ToolName::new("activate_skill"),
            json!({ "name": "reusable" }),
            SessionId::new("session"),
            TurnId::new("turn"),
        )
    };
    let mut context = owned.borrowed();

    let first = tool.invoke(request("first"), &mut context).await.unwrap();
    assert!(
        matches!(first.result.output, ToolOutput::Text(ref text) if text.contains("full instructions"))
    );
    let duplicate = tool
        .invoke(request("duplicate"), &mut context)
        .await
        .unwrap();
    let other_first = other_tool
        .invoke(request("other-first"), &mut context)
        .await
        .unwrap();
    assert!(
        matches!(duplicate.result.output, ToolOutput::Text(ref text) if text == "Skill already read.")
    );
    assert!(
        matches!(other_first.result.output, ToolOutput::Text(ref text) if text.contains("full instructions"))
    );

    registry.reset_activations();

    let reactivated = tool
        .invoke(request("reactivated"), &mut context)
        .await
        .unwrap();
    assert!(
        matches!(reactivated.result.output, ToolOutput::Text(ref text) if text.contains("full instructions"))
    );
    let other_duplicate = other_tool
        .invoke(request("other-duplicate"), &mut context)
        .await
        .unwrap();
    assert!(
        matches!(other_duplicate.result.output, ToolOutput::Text(ref text) if text == "Skill already read.")
    );
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
async fn close_tool_can_cancel_a_detached_compose() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
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
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            permissions,
            resources,
            cancellation: None,
        }),
        approved_request: None,
    };
    let call_id = ToolCallId::new("call");
    let mut context = owned.borrowed();

    assert!(
        compose
            .backgroundable
            .begin_background(true, &call_id, &mut context)
    );
    let cancellation = context.cancellation.clone().expect("job cancellation");
    assert!(!cancellation.is_cancelled());

    let close = ToolSource::get(&compose.compose, &ToolName::new("close"))
        .expect("close tool is registered");
    close
        .invoke(
            ToolRequest::new(
                ToolCallId::new("close-call"),
                ToolName::new("close"),
                json!({ "call_id": "call" }),
                session_id,
                turn_id,
            ),
            &mut context,
        )
        .await
        .unwrap();

    assert!(cancellation.is_cancelled());
    compose.backgroundable.finish_background(true, &call_id);
}

#[tokio::test]
async fn cancelling_background_compose_finishes_through_its_normal_result() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
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
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            permissions,
            resources,
            cancellation: None,
        }),
        approved_request: None,
    };
    let request = ToolRequest::new(
        ToolCallId::new("call"),
        ToolName::new("compose"),
        json!({
            "script": "return shell({ command: \"sleep 30\", timeout_seconds: 60 })",
            "background": true
        }),
        session_id,
        turn_id,
    );
    let mut context = owned.borrowed();
    let cancel = async {
        assert!(compose.backgroundable.background_jobs.cancel("call"));
    };

    let (outcome, ()) = tokio::join!(
        compose.backgroundable.invoke_outcome(request, &mut context),
        cancel
    );

    assert!(matches!(outcome, ToolExecutionOutcome::Failed(_)));
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
    assert!(prompt.contains("Keep tool output lean"));
    assert!(prompt.contains("Do not dump whole trees"));
    assert!(prompt.contains("Use compose as a dependency graph"));
    assert!(prompt.contains("use `fold` only for reductions or genuinely sequential chains"));
    assert!(prompt.contains("when it can run across a turn boundary"));
    assert!(prompt.contains("it also suits one-shot triggers"));
    assert!(prompt.contains("including launching more detached work"));
    assert!(prompt.contains("When the remaining work depends on background results, yield"));
    assert!(prompt.contains("yielding continues the task with those results"));
    assert!(prompt.contains("the user's answer need not be completed first"));
    assert!(prompt.contains("the next step needs its result in the current turn"));
    assert!(
        prompt.contains("Prefer one compose program whenever the remaining tool graph is known")
    );
    assert!(prompt.contains("keep intermediate results inside it"));
    assert!(prompt.contains("return only the bare minimum information necessary"));
    assert!(prompt.contains("start fresh subagents from concise summaries"));
    assert!(prompt.contains("close subagents when no longer needed"));
}
