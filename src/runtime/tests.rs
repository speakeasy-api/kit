use std::{sync::Arc, time::Duration};

use agentkit_core::{
    CancellationController, ItemKind, MetadataMap, Part, SessionId, ToolCallId, ToolOutput, TurnId,
};
use agentkit_task_manager::RoutingDecision;
use agentkit_tools_core::{
    AllowAllPermissions, BasicToolExecutor, OwnedToolContext, Tool, ToolExecutionOutcome,
    ToolExecutionScope, ToolExecutor, ToolName, ToolRequest, ToolSource,
};
use serde_json::{Value, json};

use super::{
    BackgroundJobs, BackgroundableCompose, DetachRegistration, Runtime, SessionRequest,
    SessionSelection, background_route, load_initial_transcript,
};

#[test]
fn resolved_reasoning_effort_reaches_root_adapter_and_kit_children() {
    let root = tempfile::tempdir().unwrap();
    let credentials = crate::credentials::CredentialStorage::Memory;
    crate::provider::store_openrouter_test_credentials(&credentials);
    let runtime = Runtime::new_with_provider_credentials_and_effort(
        root.path(),
        "test-model",
        crate::ProviderKind::OpenRouter,
        credentials,
        Some(crate::ReasoningEffort::High),
    )
    .unwrap();

    assert_eq!(
        runtime.adapter.reasoning_effort().unwrap(),
        Some(crate::ReasoningEffort::High)
    );
    assert_eq!(
        runtime.subagents.child_config().reasoning_effort,
        Some(crate::ReasoningEffort::High)
    );
}

#[test]
fn explicit_openrouter_key_reaches_runtime_adapter_and_kit_children() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new_with_provider_credentials_effort_and_openrouter_key(
        root.path(),
        "test-model",
        crate::ProviderKind::OpenRouter,
        crate::credentials::CredentialStorage::Memory,
        None,
        Some(crate::provider::OpenRouterApiKey::new("runtime-secret")),
    )
    .unwrap();

    assert_eq!(
        runtime.openrouter_api_key.as_ref().map(|key| key.as_str()),
        Some("runtime-secret")
    );
    assert_eq!(
        runtime
            .subagents
            .child_config()
            .openrouter_api_key
            .as_ref()
            .map(|key| key.as_str()),
        Some("runtime-secret")
    );
    runtime
        .adapter
        .select(crate::provider::ModelSelection::new(
            crate::ProviderKind::OpenRouter,
            "next/model",
        ))
        .unwrap();
    let debug = format!("{:?}", runtime.openrouter_api_key);
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("runtime-secret"));
}

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
    selection.finish_new(&first, configured, false, true);
    let (retry, configured) = selection.claim();
    assert_eq!(retry.id, "selected");
    assert!(
        retry.resume,
        "a transcript opened before failure is resumed"
    );
    selection.finish_new(&retry, configured, true, false);
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

#[test]
fn matching_load_reservation_releases_without_mutation_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::with_session(
        root.path(),
        "gpt-5.4",
        SessionRequest {
            id: "selected".into(),
            resume: false,
            force: true,
        },
    )
    .unwrap();

    let load = runtime.claim_session_load("selected").unwrap();
    assert!(load.request.resume);
    assert!(
        !load.request.force,
        "load must never inherit configured --force"
    );

    let concurrent_new = runtime.claim_session().unwrap();
    assert_ne!(concurrent_new.id(), "selected");
    concurrent_new.commit().unwrap();
    drop(load);

    let selected = runtime.claim_session().unwrap();
    assert_eq!(selected.id(), "selected");
    assert!(!selected.request.resume);
    assert!(selected.request.force);
}

#[test]
fn successful_matching_load_consumes_configured_selection() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::with_session(
        root.path(),
        "gpt-5.4",
        SessionRequest {
            id: "selected".into(),
            resume: true,
            force: true,
        },
    )
    .unwrap();

    let selected = runtime.claim_session_load("selected").unwrap();
    assert!(selected.request.force);
    selected.commit().unwrap();
    let next = runtime.claim_session().unwrap();
    assert_ne!(next.id(), "selected");
}

#[test]
fn nonmatching_load_never_inherits_configured_force() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::with_session(
        root.path(),
        "gpt-5.4",
        SessionRequest {
            id: "selected".into(),
            resume: true,
            force: true,
        },
    )
    .unwrap();

    let other = runtime.claim_session_load("other").unwrap();
    assert!(!other.request.force);
}

#[test]
fn concurrent_successful_matching_load_consumes_configured_selection() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::with_session(
        root.path(),
        "gpt-5.4",
        SessionRequest {
            id: "selected".into(),
            resume: true,
            force: false,
        },
    )
    .unwrap();

    let reserved = runtime.claim_session_load("selected").unwrap();
    runtime
        .claim_session_load("selected")
        .unwrap()
        .commit()
        .unwrap();
    drop(reserved);

    let next = runtime.claim_session().unwrap();
    assert_ne!(next.id(), "selected");
}

#[test]
fn nonmatching_failed_load_does_not_touch_configured_or_generated_queues() {
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

    drop(runtime.claim_session_load("other").unwrap());
    let next = runtime.claim_session().unwrap();
    assert_eq!(next.id(), "selected");
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
    let tool = ToolSource::get(&skills, &ToolName::new("skill")).unwrap();
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
    let tool = ToolSource::get(&skills, &ToolName::new("skill")).unwrap();
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
    let tool = ToolSource::get(&skills, &ToolName::new("skill")).unwrap();
    let catalog = tool.current_spec().unwrap().description;
    assert!(catalog.contains("Project version."));
    assert!(!catalog.contains("Plugin version."));
}

#[tokio::test]
async fn compose_can_load_a_skill_repeatedly() {
    let root = tempfile::tempdir().unwrap();
    write_skill(
        &root.path().join(".agents/skills/reusable"),
        "reusable",
        "Reusable skill.",
        "full instructions",
    );
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let compose = runtime.compose(0);
    let source: Arc<dyn ToolSource> = Arc::new(compose.compose.clone());
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([source]));
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
    let outcome = compose
        .backgroundable
        .invoke_outcome(
            ToolRequest::new(
                ToolCallId::new("call"),
                ToolName::new("compose"),
                json!({
                    "script": "first = skill({ name: \"reusable\" })\nsecond = skill({ name: \"reusable\" })\nreturn [first, second]"
                }),
                session_id,
                turn_id,
            ),
            &mut owned.borrowed(),
        )
        .await;

    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("skill calls did not complete through compose: {outcome:?}");
    };
    let ToolOutput::Structured(loaded) = result.result.output else {
        panic!("compose did not return structured output");
    };
    let loaded = loaded.as_array().expect("compose returned an array");
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().all(|skill| {
        skill
            .as_str()
            .is_some_and(|text| text.contains("full instructions"))
    }));
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
fn maximum_depth_compose_omits_depth_increasing_tools() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let max_depth = runtime.max_subagent_depth();

    let below_maximum = runtime.compose(max_depth - 1);
    for name in ["subagent", "fork"] {
        assert!(
            ToolSource::get(&below_maximum.compose, &ToolName::new(name)).is_some(),
            "{name} should be available below the maximum depth"
        );
    }

    let at_maximum = runtime.compose(max_depth);
    for name in ["subagent", "fork"] {
        assert!(
            ToolSource::get(&at_maximum.compose, &ToolName::new(name)).is_none(),
            "{name} should not be advertised at the maximum depth"
        );
    }
    assert!(
        ToolSource::get(&at_maximum.compose, &ToolName::new("prompt")).is_some(),
        "non-depth-increasing session tools remain available"
    );
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

    let job = compose
        .backgroundable
        .begin_background(true, &call_id, &mut context);
    let cancellation = context.cancellation.clone().expect("job cancellation");
    assert!(!cancellation.is_cancelled());
    assert_eq!(
        compose.backgroundable.background_jobs.detach("call"),
        Some(DetachRegistration::Registered),
        "an initially backgroundable call can still be detached immediately",
    );

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
    drop(job);
}

#[tokio::test]
async fn foreground_compose_can_detach_from_turn_cancellation_and_still_be_killed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let compose = runtime.compose(0);
    let executor: Arc<dyn ToolExecutor> =
        Arc::new(BasicToolExecutor::new(Vec::<Arc<dyn ToolSource>>::new()));
    let permissions = Arc::new(AllowAllPermissions);
    let resources: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
    let parent = CancellationController::new();
    let owned = OwnedToolContext {
        session_id: SessionId::new("session"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: permissions.clone(),
        resources: resources.clone(),
        cancellation: Some(parent.handle().checkpoint()),
        execution_scope: Some(ToolExecutionScope {
            executor,
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn"),
            permissions,
            resources,
            cancellation: Some(parent.handle().checkpoint()),
        }),
        approved_request: None,
    };
    let call_id = ToolCallId::new("call");
    let mut context = owned.borrowed();
    let job = compose
        .backgroundable
        .begin_background(false, &call_id, &mut context);
    let cancellation = context.cancellation.clone().expect("compose cancellation");

    assert_eq!(
        compose.backgroundable.background_jobs.detach("call"),
        Some(DetachRegistration::Registered)
    );
    parent.interrupt();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!cancellation.is_cancelled());

    assert!(compose.backgroundable.background_jobs.cancel("call"));
    assert!(cancellation.is_cancelled());
    drop(job);
    drop(context);

    let already_cancelled_id = ToolCallId::new("already-cancelled");
    let mut already_cancelled_context = owned.borrowed();
    let already_cancelled_job = compose.backgroundable.begin_background(
        false,
        &already_cancelled_id,
        &mut already_cancelled_context,
    );
    assert!(
        already_cancelled_context
            .cancellation
            .as_ref()
            .expect("compose cancellation")
            .is_cancelled()
    );
    drop(already_cancelled_job);

    assert_eq!(
        compose
            .backgroundable
            .background_jobs
            .detach("pending-detach"),
        Some(DetachRegistration::Registered)
    );
    let pending_detach_id = ToolCallId::new("pending-detach");
    let mut pending_detach_context = owned.borrowed();
    let pending_detach_job = compose.backgroundable.begin_background(
        false,
        &pending_detach_id,
        &mut pending_detach_context,
    );
    assert!(
        !pending_detach_context
            .cancellation
            .as_ref()
            .expect("compose cancellation")
            .is_cancelled()
    );
    drop(pending_detach_job);
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
fn pending_detach_is_applied_when_compose_registers() {
    let jobs = BackgroundJobs::default();

    assert_eq!(
        jobs.detach("pending-call"),
        Some(DetachRegistration::Registered)
    );
    jobs.register_foreground_for_test("pending-call");

    assert!(jobs.is_detached_for_test("pending-call"));
}

#[test]
fn duplicate_detach_does_not_take_rollback_ownership() {
    let jobs = BackgroundJobs::default();
    jobs.register_foreground_for_test("duplicate-call");

    assert_eq!(
        jobs.detach("duplicate-call"),
        Some(DetachRegistration::Registered)
    );
    let duplicate = jobs.detach("duplicate-call");
    assert_eq!(duplicate, Some(DetachRegistration::AlreadyDetached));
    if duplicate == Some(DetachRegistration::Registered) {
        jobs.restore_foreground("duplicate-call");
    }

    assert!(jobs.is_detached_for_test("duplicate-call"));
}

#[test]
fn background_terminal_publication_is_acknowledged_by_call_id() {
    let jobs = BackgroundJobs::default();

    jobs.register_foreground_for_test("finished-first");
    assert_eq!(
        jobs.detach("finished-first"),
        Some(DetachRegistration::Registered)
    );
    jobs.finish_for_test("finished-first");
    assert!(jobs.activity().unacknowledged_terminals);

    jobs.acknowledge_terminal(&ToolCallId::new("other-call"));
    assert!(jobs.activity().unacknowledged_terminals);
    jobs.acknowledge_terminal(&ToolCallId::new("finished-first"));
    assert!(!jobs.activity().unacknowledged_terminals);

    jobs.register_foreground_for_test("published-first");
    assert_eq!(
        jobs.detach("published-first"),
        Some(DetachRegistration::Registered)
    );
    jobs.acknowledge_terminal(&ToolCallId::new("published-first"));
    jobs.finish_for_test("published-first");
    assert!(!jobs.activity().unacknowledged_terminals);
}

#[test]
fn cancel_all_covers_running_and_late_background_registration() {
    let jobs = BackgroundJobs::default();
    let initial = jobs.activity();
    jobs.register_foreground_for_test("running");
    assert!(jobs.activity().active);

    jobs.cancel_all();
    assert!(jobs.is_cancelled_for_test("running"));
    jobs.register_foreground_for_test("late");
    assert!(jobs.is_cancelled_for_test("late"));

    jobs.finish_for_test("running");
    jobs.finish_for_test("late");
    let quiescent = jobs.activity();
    assert!(!quiescent.active);
    assert!(quiescent.generation > initial.generation);

    jobs.begin_turn();
    jobs.register_foreground_for_test("next-turn");
    assert!(!jobs.is_cancelled_for_test("next-turn"));
    jobs.finish_for_test("next-turn");
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
