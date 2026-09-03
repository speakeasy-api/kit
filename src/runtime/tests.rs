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
    BackgroundJobs, BackgroundableCompose, DetachRegistration, DynamicSkillTool,
    LogoutAuthenticationError, Runtime, SessionRequest, SessionSelection, background_route,
    load_initial_transcript,
};

#[tokio::test]
async fn subagent_manager_survives_runtime_reconstruction() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let runtime = Runtime::with_telemetry(runtime, Default::default()).unwrap();

    let harnesses = crate::acp_child::AcpHarnesses::default();
    let runtime =
        Runtime::with_acp_harnesses(runtime, harnesses, crate::acp_child::BUILTIN_HARNESS.into())
            .unwrap();

    let configured_directory = tempfile::tempdir().unwrap();
    let mcp_path = configured_directory.path().join("mcp.json");
    std::fs::write(&mcp_path, r#"{"mcpServers":{}}"#).unwrap();
    let runtime = Runtime::with_mcp_sources(
        runtime,
        Some(&mcp_path),
        None,
        Vec::new(),
        false,
        crate::credentials::CredentialStorage::Memory,
    )
    .await
    .unwrap();
    assert_eq!(
        runtime
            .subagents
            .child_config()
            .configured_mcp_config
            .as_deref(),
        Some(mcp_path.as_path())
    );
    assert!(
        runtime
            .subagents
            .child_config()
            .configured_mcp_config_inherited
    );
    assert!(runtime.subagents.child_config().mcp_config.is_none());
    let _fresh = runtime.subagents.fresh();
}

#[tokio::test]
async fn legacy_with_mcp_config_remains_an_explicit_layer() {
    let root = tempfile::tempdir().unwrap();
    let explicit = root.path().join("explicit.json");
    std::fs::write(&explicit, r#"{"mcpServers":{}}"#).unwrap();
    std::fs::write(
        root.path().join(".mcp.json"),
        r#"{"mcpServers":{"project-only":{"command":"missing"}}}"#,
    )
    .unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let runtime = Runtime::with_mcp_config(
        runtime,
        Some(&explicit),
        Vec::new(),
        false,
        crate::credentials::CredentialStorage::Memory,
    )
    .await
    .unwrap();

    assert!(
        runtime
            .subagents
            .child_config()
            .configured_mcp_config
            .is_none()
    );
    assert_eq!(
        runtime.subagents.child_config().mcp_config.as_deref(),
        Some(explicit.as_path())
    );
    assert!(runtime.subagents.child_config().legacy_mcp_config);
    assert!(
        !runtime
            .subagents
            .child_config()
            .configured_mcp_config_inherited
    );
    assert_eq!(
        runtime.mcp.config_source_states().await,
        vec![(explicit, true, true)]
    );
}

#[tokio::test]
async fn legacy_with_no_config_still_propagates_without_project_discovery() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(".mcp.json"),
        r#"{"mcpServers":{"project-only":{"command":"missing"}}}"#,
    )
    .unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let shared = Arc::clone(&runtime);
    let runtime = Runtime::with_mcp_config(
        runtime,
        None,
        Vec::new(),
        false,
        crate::credentials::CredentialStorage::Memory,
    )
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&runtime, &shared));
    let child = runtime.subagents.child_config();

    assert!(child.configured_mcp_config.is_none());
    assert!(!child.configured_mcp_config_inherited);
    assert!(child.legacy_mcp_config);
    assert!(child.mcp_config.is_none());
    assert!(runtime.mcp.config_source_states().await.is_empty());
}

#[tokio::test]
async fn layered_relative_paths_use_captured_launch_cwd() {
    let root = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let configured = launch.path().join("configured.json");
    let explicit = launch.path().join("explicit.json");
    std::fs::write(&configured, r#"{"mcpServers":{}}"#).unwrap();
    std::fs::write(&explicit, r#"{"mcpServers":{}}"#).unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let runtime = Runtime::with_mcp_sources_from_cwd(
        runtime,
        Some(std::path::Path::new("configured.json")),
        Some(std::path::Path::new("explicit.json")),
        launch.path(),
        Vec::new(),
        false,
        crate::credentials::CredentialStorage::Memory,
    )
    .await
    .unwrap();
    let child = runtime.subagents.child_config();

    assert_eq!(
        child.configured_mcp_config.as_deref(),
        Some(configured.as_path())
    );
    assert!(child.configured_mcp_config_inherited);
    assert!(!child.legacy_mcp_config);
    assert_eq!(child.mcp_config.as_deref(), Some(explicit.as_path()));
    assert_eq!(
        runtime.mcp.config_source_states().await,
        vec![
            (configured, true, true),
            (
                root.path().canonicalize().unwrap().join(".mcp.json"),
                false,
                false
            ),
            (explicit, true, true),
        ]
    );
}

#[tokio::test]
async fn project_mcp_source_is_canonical_root_and_tracks_absence() {
    let root = tempfile::tempdir().unwrap();
    let canonical_root = root.path().canonicalize().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let runtime = Runtime::with_mcp_sources(
        runtime,
        None,
        None,
        Vec::new(),
        false,
        crate::credentials::CredentialStorage::Memory,
    )
    .await
    .unwrap();

    assert_eq!(
        runtime.mcp.config_source_states().await,
        vec![(canonical_root.join(".mcp.json"), false, false)]
    );
    let child = runtime.subagents.child_config();
    assert!(child.configured_mcp_config.is_none());
    assert!(child.configured_mcp_config_inherited);
    assert!(!child.legacy_mcp_config);
}

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
fn openrouter_keys_disable_openrouter_authentication_and_global_logout() {
    for (explicit_key, ambient_key) in [(true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let credentials = tempfile::tempdir().unwrap();
        let mut runtime = Runtime::new_with_provider_credentials_effort_and_openrouter_key(
            root.path(),
            "gpt-5.4",
            crate::ProviderKind::OpenAiSubscription,
            crate::credentials::CredentialStorage::Filesystem(credentials.path().to_path_buf()),
            None,
            explicit_key.then(|| crate::provider::OpenRouterApiKey::new("unrelated")),
        )
        .unwrap();
        Arc::get_mut(&mut runtime)
            .unwrap()
            .set_ambient_openrouter_api_key_for_test(ambient_key);

        assert!(runtime.supports_terminal_authentication(crate::ProviderKind::OpenAiSubscription));
        assert!(!runtime.supports_terminal_authentication(crate::ProviderKind::OpenRouter));
        assert!(runtime.supports_terminal_authentication(crate::ProviderKind::Speakeasy));
        assert!(!runtime.supports_logout_authentication());
        assert!(matches!(
            runtime.logout_authentication(),
            Err(LogoutAuthenticationError::CredentialStateUnchanged(_))
        ));
    }
}

#[test]
fn logout_attempts_all_providers_after_a_credential_removal_failure() {
    let root = tempfile::tempdir().unwrap();
    let credentials = tempfile::tempdir().unwrap();
    let storage =
        crate::credentials::CredentialStorage::Filesystem(credentials.path().to_path_buf());
    storage.make_entry_undeletable_for_test("openrouter", "default");
    storage
        .entry("speakeasy", "default")
        .save(b"removed")
        .unwrap();
    let mut runtime = Runtime::new_with_provider_and_credentials(
        root.path(),
        "gpt-5.4",
        crate::ProviderKind::OpenAiSubscription,
        storage.clone(),
    )
    .unwrap();
    Arc::get_mut(&mut runtime)
        .unwrap()
        .set_ambient_openrouter_api_key_for_test(false);

    let error = runtime.logout_authentication().unwrap_err();

    let LogoutAuthenticationError::CredentialStateMayHaveChanged(message) = error else {
        panic!("credential removal failure must report possibly changed state");
    };
    assert!(message.contains("could not remove OpenRouter credentials"));
    assert!(
        storage
            .entry("speakeasy", "default")
            .load()
            .unwrap()
            .is_none()
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
    selection.finish_new(&first, configured, false, true, false);
    let (retry, configured) = selection.claim();
    assert_eq!(retry.id, "selected");
    assert!(
        retry.resume,
        "a transcript opened before failure is resumed"
    );
    selection.finish_new(&retry, configured, true, false, false);
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
fn dropped_uncommitted_session_claim_retries_as_new() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();

    let mut failed = runtime.claim_session().unwrap();
    let session_id = failed.id().to_owned();
    let opened = crate::session::open_uncommitted(
        root.path(),
        &session_id,
        false,
        vec![agentkit_core::Item::text(ItemKind::System, "system")],
    )
    .unwrap();
    failed.guard_uncommitted_transcript(&opened.observer);
    drop(opened);
    drop(failed);

    assert!(crate::session::load(root.path(), &session_id).is_err());
    let retry = runtime.claim_session().unwrap();
    assert_eq!(retry.id(), session_id);
    assert!(!retry.request.resume);
}

#[test]
fn uncommitted_claim_rolls_back_before_waiting_to_publish_retry() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let mut failed = runtime.claim_session().unwrap();
    let session_id = failed.id().to_owned();
    let opened = crate::session::open_uncommitted(
        root.path(),
        &session_id,
        false,
        vec![agentkit_core::Item::text(ItemKind::System, "system")],
    )
    .unwrap();
    failed.guard_uncommitted_transcript(&opened.observer);
    drop(opened);

    let selection = runtime.session.lock().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let dropping = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        drop(failed);
    });
    started_rx.recv().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while crate::session::load(root.path(), &session_id).is_ok()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        crate::session::load(root.path(), &session_id).is_err(),
        "uncommitted transcript must roll back before the retry mutex is available"
    );

    drop(selection);
    dropping.join().unwrap();
    let retry = runtime.claim_session().unwrap();
    assert_eq!(retry.id(), session_id);
}

#[test]
fn dropped_fork_claim_removes_its_uncommitted_transcript() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();

    let mut failed = runtime.claim_session_fork().unwrap();
    let session_id = failed.id().to_owned();
    let opened = crate::session::open_uncommitted(
        root.path(),
        &session_id,
        false,
        vec![agentkit_core::Item::text(ItemKind::System, "system")],
    )
    .unwrap();
    failed.guard_uncommitted_transcript(&opened.observer);
    drop(opened);
    assert!(crate::session::load(root.path(), &session_id).is_ok());

    drop(failed);

    assert!(crate::session::load(root.path(), &session_id).is_err());
}

#[test]
fn dropped_deferred_fork_creation_removes_transcript_before_response() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();

    let mut claim = runtime.claim_session_fork().unwrap();
    let session_id = claim.id().to_owned();
    let opened = crate::session::open_uncommitted(
        root.path(),
        &session_id,
        false,
        vec![agentkit_core::Item::text(ItemKind::System, "system")],
    )
    .unwrap();
    claim.guard_uncommitted_transcript(&opened.observer);
    let creation = claim.defer_fork_commit();
    drop(opened);
    drop(creation);

    assert!(crate::session::load(root.path(), &session_id).is_err());
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
async fn initially_empty_live_plugin_skill_tool_becomes_actionable_and_tracks_changes() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let plugins = crate::plugins::PluginRuntime::load(
        config.clone(),
        root.path().to_path_buf(),
        root.path().join("cache"),
        root.path().join("data"),
    )
    .await
    .unwrap();
    let runtime = Runtime::with_plugin_runtime(
        Runtime::new(root.path(), "gpt-5.4").unwrap(),
        Some(plugins.clone()),
    )
    .unwrap();
    let runtime = Runtime::with_mcp_config(
        runtime,
        None,
        Vec::new(),
        false,
        crate::tools::mcp::CredentialStorage::Memory,
    )
    .await
    .unwrap();
    let compose = runtime.compose(0);
    let tool = DynamicSkillTool::new(root.path().to_path_buf(), plugins.clone()).unwrap();
    assert_eq!(
        tool.spec().input_schema["properties"]["name"]["type"],
        "string"
    );
    assert!(
        tool.spec().input_schema["properties"]["name"]
            .get("enum")
            .is_none()
    );
    let initial = tool
        .current_spec()
        .expect("the skill tool remains visible with an empty plugin catalog");
    assert!(!initial.input_schema.to_string().contains("live-skill"));

    let package = root.path().join("plugin");
    let skill = package.join("skills/live-skill");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"live-plugin"}"#,
    )
    .unwrap();
    write_skill(&skill, "live-skill", "Live skill.", "first body");
    std::fs::write(
        &config,
        format!(
            "[plugins.live]\nsource = 'path'\npath = '{}'\n",
            package.display()
        ),
    )
    .unwrap();
    let refreshed = runtime.current_skills().await.unwrap();
    assert!(
        refreshed
            .skills
            .iter()
            .any(|skill| skill.name == "live-skill")
    );
    drop(refreshed);
    assert!(
        tool.current_spec()
            .unwrap()
            .input_schema
            .to_string()
            .contains("live-skill")
    );
    assert!(
        tool.spec().input_schema["properties"]["name"]
            .get("enum")
            .is_none(),
        "the frozen model schema remains open as the catalog changes"
    );
    assert!(compose.specs()[0].description.contains("live-skill"));

    let session_id = SessionId::new("session");
    let turn_id = TurnId::new("turn");
    let owned = OwnedToolContext {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let invoke = |id: &str| {
        ToolRequest::new(
            ToolCallId::new(id),
            ToolName::new("skill"),
            json!({"name": "live-skill"}),
            session_id.clone(),
            turn_id.clone(),
        )
    };
    let first = tool
        .invoke(invoke("first"), &mut owned.borrowed())
        .await
        .unwrap();
    assert!(format!("{:?}", first.result.output).contains("first body"));

    let published = plugins.snapshot();
    let generation = published.package_roots[0]
        .parent()
        .expect("snapshot package has a generation root");
    crate::plugins::make_tree_writable_for_test(generation);
    std::fs::write(
        published.skill_directories[0].join("SKILL.md"),
        "poisoned cache body",
    )
    .unwrap();
    let still_immutable = tool
        .invoke(invoke("cache-poison"), &mut owned.borrowed())
        .await
        .unwrap();
    assert!(format!("{:?}", still_immutable.result.output).contains("first body"));
    assert!(!format!("{:?}", still_immutable.result.output).contains("poisoned"));
    drop(published);

    write_skill(&skill, "live-skill", "Live skill.", "second body");
    let still_first = tool
        .invoke(invoke("before-refresh"), &mut owned.borrowed())
        .await
        .unwrap();
    assert!(format!("{:?}", still_first.result.output).contains("first body"));
    let refreshed = runtime.current_skills().await.unwrap();
    assert!(
        refreshed
            .skills
            .iter()
            .any(|skill| skill.body.contains("second body"))
    );
    drop(refreshed);
    let second = tool
        .invoke(invoke("second"), &mut owned.borrowed())
        .await
        .unwrap();
    assert!(format!("{:?}", second.result.output).contains("second body"));

    std::fs::write(&config, "").unwrap();
    drop(runtime.current_skills().await.unwrap());
    assert!(!compose.specs()[0].description.contains("live-skill"));
    assert!(
        tool.invoke(invoke("removed"), &mut owned.borrowed())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dynamic_skill_catalog_reads_survive_a_held_generation_writer() {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("plugin");
    let skill = package.join("skills/plugin-skill");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"plugin"}"#,
    )
    .unwrap();
    write_skill(&skill, "plugin-skill", "Plugin skill.", "plugin body");
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[plugins.live]\nsource = 'path'\npath = '{}'\n",
            package.display()
        ),
    )
    .unwrap();
    let plugins = crate::plugins::PluginRuntime::load(
        config,
        root.path().to_path_buf(),
        root.path().join("cache"),
        root.path().join("data"),
    )
    .await
    .unwrap();
    let tool = DynamicSkillTool::new(root.path().to_path_buf(), plugins.clone()).unwrap();
    let request = ToolRequest::new(
        ToolCallId::new("call"),
        ToolName::new("skill"),
        json!({"name": "plugin-skill"}),
        SessionId::new("session"),
        TurnId::new("turn"),
    );

    let generation_writer = plugins.generation_writer().await;
    let current = tool
        .current_spec()
        .expect("a writer must not hide the last published skill catalog");
    assert!(current.input_schema.to_string().contains("plugin-skill"));
    assert!(tool.proposed_requests(&request).is_ok());
    drop(generation_writer);
}

#[tokio::test]
async fn compose_can_load_a_skill_added_after_its_spec_is_frozen() {
    let root = tempfile::tempdir().unwrap();
    write_skill(
        &root.path().join(".agents/skills/reusable"),
        "reusable",
        "Reusable skill.",
        "full instructions",
    );
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let compose = runtime.compose(0);
    let frozen_description = &compose.specs()[0].description;
    assert!(frozen_description.contains("- name: reusable"));
    assert!(!frozen_description.contains(r#""enum":["reusable"]"#));
    write_skill(
        &root.path().join(".agents/skills/added-later"),
        "added-later",
        "Added after the Compose schema was frozen.",
        "new instructions",
    );
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
                    "script": "first = skill({ name: \"reusable\" })\nsecond = skill({ name: \"reusable\" })\nthird = skill({ name: \"added-later\" })\nreturn [first, second, third]"
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
    assert_eq!(loaded.len(), 3);
    assert!(loaded[..2].iter().all(|skill| {
        skill
            .as_str()
            .is_some_and(|text| text.contains("full instructions"))
    }));
    assert!(
        loaded[2]
            .as_str()
            .is_some_and(|text| text.contains("new instructions"))
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
        specs[0].input_schema["properties"]["intent"]["type"],
        "string"
    );
    assert!(
        specs[0].input_schema["properties"]["intent"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("short user-facing sentence"))
    );
    assert!(
        specs[0].input_schema["required"]
            .as_array()
            .is_none_or(|required| !required.contains(&json!("intent")))
    );
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

#[tokio::test]
async fn exact_mcp_name_cannot_bypass_the_tool_meta_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let compose = runtime.compose(0);
    assert!(ToolSource::get(&compose.compose, &ToolName::new("mcp_exact_tool")).is_none());
    let session_id = SessionId::new("session");
    let turn_id = TurnId::new("turn");
    let owned = OwnedToolContext {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let outcome = compose
        .backgroundable
        .invoke_outcome(
            ToolRequest::new(
                ToolCallId::new("call"),
                ToolName::new("compose"),
                json!({"script": "return mcp_exact_tool({})"}),
                session_id,
                turn_id,
            ),
            &mut owned.borrowed(),
        )
        .await;
    assert!(matches!(
        outcome,
        ToolExecutionOutcome::FailedBeforeInvocation(_) | ToolExecutionOutcome::Failed(_)
    ));
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

    let subagent_description = ToolSource::get(&below_maximum.compose, &ToolName::new("subagent"))
        .unwrap()
        .current_spec()
        .unwrap()
        .description;
    assert!(subagent_description.contains(
        "Use this only if you uncover independent workstreams whose parallel execution would yield quicker or better results."
    ));
    let fork_description = ToolSource::get(&below_maximum.compose, &ToolName::new("fork"))
        .unwrap()
        .current_spec()
        .unwrap()
        .description;
    assert!(fork_description.contains(
        "Use this only for an independent workstream whose parallel execution would yield quicker or better results"
    ));

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

    let with_intent = |intent| {
        ToolRequest::new(
            ToolCallId::new("call"),
            ToolName::new("compose"),
            json!({"script": "return 7", "intent": intent}),
            SessionId::new("session"),
            TurnId::new("turn"),
        )
    };
    let sanitized =
        BackgroundableCompose::sanitized(with_intent(json!("Check the runtime behavior.")))
            .unwrap();
    assert!(sanitized.input.get("intent").is_none());
    for invalid in [json!(7), json!(true), json!(null), json!({})] {
        assert!(BackgroundableCompose::sanitized(with_intent(invalid)).is_err());
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

#[tokio::test]
async fn persistent_startup_failure_does_not_commit_new_session() {
    let root = tempfile::tempdir().unwrap();
    let session_id = crate::session::new_id();
    let runtime = Runtime::with_session_provider_credentials_effort_and_openrouter_key(
        root.path(),
        "test/model",
        crate::provider::ProviderKind::OpenRouter,
        SessionRequest {
            id: session_id.clone(),
            resume: false,
            force: false,
        },
        crate::credentials::CredentialStorage::Memory,
        None,
        Some(crate::provider::OpenRouterApiKey::new("")),
    )
    .unwrap();

    let error = runtime.run_persistent("hello".into()).await.unwrap_err();
    assert!(
        error.contains("--openrouter-api-key cannot be empty"),
        "{error}"
    );
    assert!(crate::session::load(root.path(), &session_id).is_err());
}

#[tokio::test]
async fn persistent_missing_openai_credentials_do_not_commit_new_session() {
    let root = tempfile::tempdir().unwrap();
    let session_id = crate::session::new_id();
    let runtime = Runtime::with_session_provider_and_credentials(
        root.path(),
        "gpt-5.4",
        crate::provider::ProviderKind::OpenAiSubscription,
        SessionRequest {
            id: session_id.clone(),
            resume: false,
            force: false,
        },
        crate::credentials::CredentialStorage::Memory,
    )
    .unwrap();

    let error = runtime.run_persistent("hello".into()).await.unwrap_err();
    assert!(error.contains("openai_auth_required:"), "{error}");
    assert!(crate::session::load(root.path(), &session_id).is_err());
}

#[tokio::test]
async fn initial_transcript_records_structured_session_origin() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();

    for (depth, expected) in [
        (0, crate::session::TOP_LEVEL_SESSION_ORIGIN),
        (1, crate::session::SUBAGENT_SESSION_ORIGIN),
        (
            runtime.max_subagent_depth(),
            crate::session::SUBAGENT_SESSION_ORIGIN,
        ),
    ] {
        let transcript = runtime.initial_transcript(depth).await.unwrap();
        assert_eq!(
            transcript[0]
                .metadata
                .get(crate::session::SESSION_ORIGIN_METADATA_KEY),
            Some(&Value::String(expected.into()))
        );
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
    let delegated_prompt = runtime.system_prompt(1);
    assert!(delegated_prompt.contains(
        "This task was delegated to you by the primary agent. Investigate it and carry out the work."
    ));
    assert!(!prompt.contains("This task was delegated to you by the primary agent."));
    let max_depth_prompt = runtime.system_prompt(runtime.max_subagent_depth());
    assert!(max_depth_prompt.contains("This task was delegated to you by the primary agent."));
}
