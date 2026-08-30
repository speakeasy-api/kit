use std::path::{Path, PathBuf};

use super::*;

fn listing_id(value: &SubagentListing) -> &str {
    &value.id
}

type EventLog = Arc<Mutex<Vec<events::RuntimeEvent>>>;

fn observe_events(mut manager: Subagents) -> (Subagents, EventLog) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    manager.event_sink = Arc::new(move |event| {
        captured.lock().unwrap().push(event.clone());
        Ok(())
    });
    (manager, events)
}

fn emitted(events: &EventLog) -> Vec<events::RuntimeEvent> {
    events.lock().unwrap().clone()
}

impl Subagents {
    fn with_event_sink_for_test(mut self, event_sink: EventSink) -> Self {
        self.event_sink = event_sink;
        self
    }

    async fn insert_starting_for_test(&self) -> String {
        let now = events::now_millis();
        let state = self
            .insert_starting(
                session::new_id(),
                State {
                    name: String::new(),
                    status: SubagentStatus::Starting,
                    task: "test".into(),
                    generation: 1,
                    handle_generation: 1,
                    outcome: None,
                    created_at_unix_ms: now,
                    generation_started_at_unix_ms: now,
                    generation_finished_at_unix_ms: None,
                    output: Value::Null,
                    updates: None,
                    harness: crate::acp_child::BUILTIN_HARNESS.into(),
                    model: None,
                    kit: true,
                    root: self.config.root.clone(),
                    child: None,
                    forking: None,
                    permit: Some(self.reserve().unwrap()),
                },
            )
            .unwrap();
        state.lock().await.name.clone()
    }
}

#[test]
fn preferred_name_is_trimmed_and_reserved() {
    let mut used = HashSet::new();

    assert_eq!(
        allocate_display_name(&mut used, Some("  Round 2 Implementer  ")).unwrap(),
        "Round 2 Implementer"
    );
    assert!(used.contains("round 2 implementer"));
}

#[test]
fn preferred_name_collisions_receive_case_insensitive_numeric_suffixes() {
    let mut used = HashSet::from(["round 2 reviewer".into(), "round 2 reviewer 2".into()]);

    assert_eq!(
        allocate_display_name(&mut used, Some("Round 2 Reviewer")).unwrap(),
        "Round 2 Reviewer 3"
    );
}

#[test]
fn collision_suffix_keeps_name_within_32_bytes() {
    let base = "A1234567890123456789012345678901";
    let mut used = HashSet::from([base.to_lowercase()]);

    let allocated = allocate_display_name(&mut used, Some(base)).unwrap();

    assert_eq!(allocated, "A12345678901234567890123456789 2");
    assert_eq!(allocated.len(), 32);
}

#[test]
fn bounded_name_allocation_reports_an_exhausted_fallback_space() {
    let mut used = (1..=MAX_LIVE_SUBAGENTS + 1)
        .map(|number| format!("agent {number}"))
        .collect();

    assert!(allocate_display_name(&mut used, None).is_err());
}

#[test]
fn missing_or_invalid_preferred_name_uses_lowest_available_agent_fallback() {
    for candidate in [None, Some(""), Some("bad\nname"), Some("évaluator")] {
        let mut used = HashSet::from(["agent 1".into()]);
        assert_eq!(
            allocate_display_name(&mut used, candidate).unwrap(),
            "Agent 2"
        );
    }
}

#[test]
fn name_reservations_are_case_insensitive_and_reusable_after_release() {
    let mut used = HashSet::new();
    assert_eq!(
        allocate_display_name(&mut used, Some("Reviewer")).unwrap(),
        "Reviewer"
    );
    assert_eq!(
        allocate_display_name(&mut used, Some("reviewer")).unwrap(),
        "reviewer 2"
    );

    assert!(used.remove("reviewer"));
    assert_eq!(
        allocate_display_name(&mut used, Some("Reviewer")).unwrap(),
        "Reviewer"
    );
}

#[tokio::test]
async fn manager_name_allocation_is_atomic_across_concurrent_insertions() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_generic_harness(root.path(), vec!["--no-fork".into()]);
    let starts = (0..8)
        .map(|_| {
            let manager = manager.clone();
            tokio::spawn(async move { manager.insert_starting_for_test().await })
        })
        .collect::<Vec<_>>();
    let mut allocated = HashSet::new();
    for start in starts {
        allocated.insert(start.await.unwrap().to_lowercase());
    }

    assert_eq!(allocated.len(), 8);
    assert_eq!(manager.sessions.lock().unwrap().len(), 8);
}

#[test]
fn name_allocation_summarizes_tasks_as_single_bounded_lines() {
    assert_eq!(task_summary("  trace\n\t the   flow  "), "trace the flow");
    assert_eq!(task_summary(" \n\t "), "Untitled task");
    let summary = task_summary(&"é".repeat(97));
    assert_eq!(summary.chars().count(), 96);
    assert!(summary.ends_with('…'));
}

#[test]
fn subagent_value_reads_legacy_and_current_handles_and_rejects_malformed_shapes() {
    let legacy = serde_json::from_value::<SubagentValue>(json!({
        "id": "s-old", "output": null, "generation": 1
    }))
    .expect("legacy handle remains readable");
    assert_eq!(legacy.name, None);

    let current = serde_json::from_value::<SubagentValue>(json!({
        "id": "s-current", "name": "Scout", "output": "done", "generation": 2
    }))
    .expect("current handle remains readable");
    assert_eq!(current.name.as_deref(), Some("Scout"));
    assert!(
        serde_json::from_value::<SubagentValue>(json!({
            "id": "s-bad", "name": 42, "output": null, "generation": 1
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SubagentValue>(json!({
            "id": "s-bad", "output": null, "generation": 1, "extra": true
        }))
        .is_err()
    );
}

#[test]
fn subagent_value_schema_accepts_legacy_and_current_handles() {
    let validator = jsonschema::validator_for(&value_schema()).unwrap();
    assert!(validator.is_valid(&json!({
        "id": "s-old", "output": null, "generation": 1
    })));
    assert!(validator.is_valid(&json!({
        "id": "s-new", "name": "Scout", "output": null, "generation": 1
    })));
    assert!(!validator.is_valid(&json!({
        "id": "s-bad", "name": 7, "output": null, "generation": 1
    })));
}

#[test]
fn model_inputs_prefer_optional_subagent_names() {
    let input = serde_json::from_value::<Input>(json!({
        "prompt": "work",
        "name": "Implementer",
        "cwd": "../other-worktree"
    }))
    .unwrap();
    assert_eq!(input.name.as_deref(), Some("Implementer"));
    assert_eq!(input.cwd, Some(PathBuf::from("../other-worktree")));
    assert!(
        serde_json::from_value::<ForkInput>(json!({
            "subagent": {"id": "s", "output": null, "generation": 1},
            "prompt": "fork work",
            "name": "Round 2 Reviewer"
        }))
        .is_ok()
    );
    let directory = tempfile::tempdir().unwrap();
    let manager = manager_with_generic_harness(directory.path(), Vec::new());
    let subagent = SubagentTool::new(manager.clone(), 1);
    let fork = ForkTool::new(manager, 1);
    let cwd = &subagent.spec.input_schema["properties"]["cwd"];
    assert_eq!(cwd["type"], "string");
    assert_eq!(cwd["minLength"], 1);
    assert!(
        cwd["description"]
            .as_str()
            .unwrap()
            .contains("Relative paths resolve from Kit's working directory")
    );
    assert!(fork.spec.input_schema["properties"].get("cwd").is_none());

    for schema in [&subagent.spec.input_schema, &fork.spec.input_schema] {
        let name = &schema["properties"]["name"];
        assert!(name.get("maxLength").is_none());
        let guidance = name["description"].as_str().unwrap();
        assert!(guidance.contains("unique among live sibling subagents"));
        assert!(guidance.contains("1-32 bytes of printable ASCII"));
        assert!(!guidance.contains('`'));
    }
}

#[test]
fn structured_output_is_parsed_and_validated() {
    let contract = OutputContract::new(json!({
        "type": "object",
        "properties": {"approved": {"type": "boolean"}},
        "required": ["approved"],
        "additionalProperties": false
    }))
    .unwrap();

    assert_eq!(
        contract.parse(r#"{"approved":true}"#).unwrap(),
        json!({"approved": true})
    );
    assert!(contract.parse(r#"{"approved":"yes"}"#).is_none());
    assert!(contract.parse("```json\n{}\n```").is_none());
}

#[test]
fn invalid_output_schema_is_rejected() {
    assert!(OutputContract::new(json!({"type": 42})).is_err());
}

#[test]
fn explicit_null_output_schema_is_rejected() {
    assert!(
        serde_json::from_value::<Input>(json!({"prompt": "test", "output_schema": null})).is_err()
    );
}

#[test]
fn boolean_output_schema_is_supported() {
    let contract = OutputContract::new(Value::Bool(true)).unwrap();
    assert_eq!(contract.parse("[1, 2]").unwrap(), json!([1, 2]));
}

#[test]
fn unstructured_output_remains_text() {
    assert_eq!(
        structured_output("plain text".into(), None),
        Value::String("plain text".into())
    );
}

#[test]
fn text_only_values_keep_the_existing_json_shape() {
    let value = SubagentValue {
        id: "child".into(),
        name: None,
        output: Value::String("done".into()),
        generation: 1,
        updates: None,
    };

    assert_eq!(
        serde_json::to_value(value).unwrap(),
        json!({"id": "child", "output": "done", "generation": 1})
    );
}

fn manager_with_disconnected_session(
    root: &Path,
) -> (Subagents, Arc<AsyncMutex<State>>, SubagentValue) {
    let manager = Subagents::new(
        ChildConfig {
            root: root.to_path_buf(),
            model: "test".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses: Default::default(),
            default_harness: crate::acp_child::BUILTIN_HARNESS.into(),
            parent_id: None,
            parent_name: None,
        },
        2,
    );
    let state = Arc::new(AsyncMutex::new(State {
        name: "Scout".into(),
        status: SubagentStatus::Idle,
        task: "done".into(),
        generation: 1,
        handle_generation: 1,
        outcome: Some(GenerationOutcome::Success),
        created_at_unix_ms: 1,
        generation_started_at_unix_ms: 1,
        generation_finished_at_unix_ms: Some(2),
        output: Value::String("done".into()),
        updates: None,
        harness: crate::acp_child::BUILTIN_HARNESS.into(),
        model: None,
        kit: true,
        root: root.to_path_buf(),
        child: Some(ChildSession::disconnected_for_test()),
        forking: None,
        permit: Some(Arc::clone(&manager.capacity).try_acquire_owned().unwrap()),
    }));
    manager.sessions.lock().unwrap().insert(
        "source".into(),
        SessionEntry {
            name: "Scout".into(),
            state: Arc::clone(&state),
        },
    );
    let prior = SubagentValue {
        id: "source".into(),
        name: Some("Scout".into()),
        output: Value::String("done".into()),
        generation: 1,
        updates: None,
    };
    (manager, state, prior)
}

#[tokio::test]
async fn close_does_not_block_listings_or_allow_stale_reuse() {
    let root = tempfile::tempdir().unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
        "generic".into(),
        crate::acp_child::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture, "--slow-close".into()],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let manager = Subagents::new(
        ChildConfig {
            root: root.path().to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses,
            default_harness: "acp.generic".into(),
            parent_id: None,
            parent_name: None,
        },
        2,
    );
    let handle = manager
        .create(
            "base".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    let close_manager = manager.clone();
    let close_id = handle.id.clone();
    let close = tokio::spawn(async move {
        close_manager
            .close(&close_id, &TurnCancellation::default())
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let listing = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        manager.list(&TurnCancellation::default()),
    )
    .await
    .expect("listing blocked behind child close")
    .unwrap();
    assert!(listing.is_empty());
    let reuse = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        manager.prompt(
            handle,
            "stale reuse".into(),
            TurnCancellation::default(),
            None,
        ),
    )
    .await
    .expect("reuse blocked behind child close");
    assert!(reuse.is_err());
    close.await.unwrap().unwrap();
}

#[tokio::test]
async fn listing_omits_closed_subagents() {
    let directory = tempfile::tempdir().unwrap();
    let (manager, state, prior) = manager_with_disconnected_session(directory.path());
    let (child, closed) = ChildSession::closure_probe_for_test();
    state.lock().await.child = Some(child);
    drop(state);

    let cancellation = TurnCancellation::default();
    assert_eq!(manager.list(&cancellation).await.unwrap().len(), 1);
    assert_eq!(
        listing_id(&manager.list(&cancellation).await.unwrap()[0]),
        prior.id
    );
    manager.close(&prior.id, &cancellation).await.unwrap();
    assert!(manager.list(&cancellation).await.unwrap().is_empty());
    assert!(manager.lookup(&prior).is_err());
    tokio::time::timeout(std::time::Duration::from_secs(1), closed)
        .await
        .expect("closing did not terminate the child actor")
        .unwrap();
}

#[tokio::test]
async fn dropping_a_session_manager_terminates_its_children() {
    let directory = tempfile::tempdir().unwrap();
    let (manager, state, _) = manager_with_disconnected_session(directory.path());
    let (child, closed) = ChildSession::closure_probe_for_test();
    state.lock().await.child = Some(child);
    drop(state);

    drop(manager);

    tokio::time::timeout(std::time::Duration::from_secs(1), closed)
        .await
        .expect("manager drop did not terminate the child actor")
        .unwrap();
}

#[test]
fn close_input_accepts_a_handle_subagent_id_or_background_call_id() {
    let handle = json!({"id": "child", "output": "done", "generation": 1});
    let id = json!({"id": "child"});
    let call_id = json!({"call_id": "call_123"});
    assert_eq!(
        serde_json::from_value::<CloseInput>(handle)
            .unwrap()
            .target()
            .0,
        "child"
    );
    assert_eq!(
        serde_json::from_value::<CloseInput>(id).unwrap().target().0,
        "child"
    );
    assert_eq!(
        serde_json::from_value::<CloseInput>(call_id)
            .unwrap()
            .target(),
        ("call_123".into(), true)
    );
    assert!(serde_json::from_value::<CloseInput>(json!({"id": "child", "extra": true})).is_err());
}

#[test]
fn live_session_limit_is_120_per_manager() {
    let directory = tempfile::tempdir().unwrap();
    let (manager, _, prior) = manager_with_disconnected_session(directory.path());
    manager.sessions.lock().unwrap().remove(&prior.id);
    let permits = (0..MAX_LIVE_SUBAGENTS)
        .map(|_| manager.reserve().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(permits.len(), 120);
    assert_eq!(
        manager.reserve().unwrap_err().to_string(),
        "live subagent session limit (120) reached"
    );
}

#[test]
fn invalid_structured_output_remains_recoverable_as_text() {
    let contract = OutputContract::new(json!({"type": "object"})).unwrap();

    assert_eq!(
        structured_output("not JSON".into(), Some(&contract)),
        Value::String("not JSON".into())
    );
}

fn manager_with_generic_harness(root: &Path, args: Vec<String>) -> Subagents {
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
        "generic".into(),
        crate::acp_child::AcpHarnessProfile {
            command: "python3".into(),
            args: std::iter::once(fixture).chain(args).collect(),
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    Subagents::new(
        ChildConfig {
            root: root.to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses,
            default_harness: "acp.generic".into(),
            parent_id: None,
            parent_name: None,
        },
        2,
    )
}

#[test]
fn selected_root_keeps_inherited_relative_paths_based_on_parent_root() {
    let parent = tempfile::tempdir().unwrap();
    let child = tempfile::tempdir().unwrap();
    let mut config = manager_with_generic_harness(parent.path(), Vec::new()).child_config();
    config.mcp_config = Some(PathBuf::from("config/mcp.json"));
    config.credential_storage =
        crate::credentials::CredentialStorage::Filesystem(PathBuf::from("credentials"));

    let config = config.with_root(child.path().to_path_buf());

    assert_eq!(
        config.mcp_config,
        Some(parent.path().join("config/mcp.json"))
    );
    assert_eq!(
        config.credential_storage.directory(),
        Some(parent.path().join("credentials").as_path())
    );
    assert_eq!(config.root, child.path());
}

#[tokio::test]
async fn create_uses_requested_working_directory_without_changing_parent() {
    let root = tempfile::tempdir().unwrap();
    let child_root = root.path().join("other-worktree");
    std::fs::create_dir(&child_root).unwrap();
    let child_root = child_root.canonicalize().unwrap();
    let requests = root.path().join("requests.jsonl");
    let manager = manager_with_generic_harness(
        root.path(),
        vec![fixture_path_arg("--request-log", &requests)],
    );

    let source = manager
        .create(
            "MOCK_CWD".into(),
            CreateOptions {
                cwd: Some(PathBuf::from("other-worktree")),
                ..Default::default()
            },
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        source.output,
        Value::String(child_root.display().to_string())
    );
    assert_eq!(manager.config.root, root.path());
    assert_eq!(
        manager.lookup(&source).unwrap().lock().await.root,
        child_root
    );
    wait_for_logged(
        &requests,
        |request| matches!(request, LoggedRequest::New { cwd } if cwd == &child_root),
    )
    .await;

    let branch = manager
        .fork(
            source.clone(),
            "MOCK_CWD".into(),
            None,
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        branch.output,
        Value::String(child_root.display().to_string())
    );
    assert_eq!(
        manager.lookup(&branch).unwrap().lock().await.root,
        child_root
    );
    wait_for_logged(
        &requests,
        |request| matches!(request, LoggedRequest::Fork { cwd, .. } if cwd == &child_root),
    )
    .await;

    manager
        .close(&branch.id, &TurnCancellation::default())
        .await
        .unwrap();
    manager
        .close(&source.id, &TurnCancellation::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn create_rejects_invalid_requested_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    std::fs::write(&file, "not a directory").unwrap();
    let manager = manager_with_generic_harness(root.path(), Vec::new());

    let missing = manager
        .create(
            "work".into(),
            CreateOptions {
                cwd: Some(PathBuf::from("missing")),
                ..Default::default()
            },
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("could not open subagent working directory")
    );

    let not_directory = manager
        .create(
            "work".into(),
            CreateOptions {
                cwd: Some(file),
                ..Default::default()
            },
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        not_directory
            .to_string()
            .contains("subagent working directory is not a directory")
    );
    assert_eq!(manager.capacity.available_permits(), MAX_LIVE_SUBAGENTS);
}

fn fixture_path_arg(name: &str, path: &Path) -> String {
    format!("{name}={}", path.display())
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "method")]
enum LoggedRequest {
    #[serde(rename = "session/new")]
    New { cwd: PathBuf },
    #[serde(rename = "session/fork")]
    Fork {
        #[serde(rename = "sessionId")]
        session_id: String,
        cwd: PathBuf,
    },
    #[serde(rename = "session/prompt")]
    Prompt {
        #[serde(rename = "sessionId")]
        session_id: String,
        text: String,
    },
    #[serde(rename = "session/close")]
    Close {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(other)]
    Other,
}

fn logged_requests(path: &Path) -> Vec<LoggedRequest> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn wait_for_logged(
    path: &Path,
    matches: impl Fn(&LoggedRequest) -> bool,
) -> Vec<LoggedRequest> {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let requests = logged_requests(path);
            if requests.iter().any(&matches) {
                return requests;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mock ACP request was not observed")
}

async fn wait_for_available_permits(manager: &Subagents, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if manager.capacity.available_permits() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("subagent capacity was not released");
}

#[derive(Default)]
struct ScenarioOptions {
    gate_new: bool,
    gate_fork: bool,
    gate_prompt: Option<&'static str>,
    fail_close_session: Option<&'static str>,
}

struct MockAcpScenario {
    _root: tempfile::TempDir,
    manager: Subagents,
    requests: std::path::PathBuf,
    new_release: std::path::PathBuf,
    fork_release: std::path::PathBuf,
    prompt_release: std::path::PathBuf,
}

impl MockAcpScenario {
    fn new(options: ScenarioOptions) -> Self {
        let root = tempfile::tempdir().unwrap();
        let requests = root.path().join("requests.jsonl");
        let new_release = root.path().join("release-new");
        let fork_release = root.path().join("release-fork");
        let prompt_release = root.path().join("release-prompt");
        let mut args = vec![fixture_path_arg("--request-log", &requests)];
        if options.gate_new {
            args.push(fixture_path_arg("--new-release", &new_release));
        }
        if options.gate_fork {
            args.push(fixture_path_arg("--fork-release", &fork_release));
        }
        if let Some(text) = options.gate_prompt {
            args.push(fixture_path_arg("--prompt-release", &prompt_release));
            args.push(format!("--prompt-release-text={text}"));
        }
        if let Some(session_id) = options.fail_close_session {
            args.push(format!("--fail-close-session={session_id}"));
        }
        let manager = manager_with_generic_harness(root.path(), args);
        Self {
            _root: root,
            manager,
            requests,
            new_release,
            fork_release,
            prompt_release,
        }
    }

    async fn create(&self, prompt: &str) -> SubagentValue {
        self.manager
            .create(
                prompt.into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap()
    }

    fn spawn_fork(
        &self,
        source: SubagentValue,
        prompt: &'static str,
    ) -> tokio::task::JoinHandle<Result<SubagentValue, ChildError>> {
        let manager = self.manager.clone();
        tokio::spawn(async move {
            manager
                .fork(
                    source,
                    prompt.into(),
                    None,
                    0,
                    TurnCancellation::default(),
                    None,
                )
                .await
        })
    }

    async fn wait_for(&self, matches: impl Fn(&LoggedRequest) -> bool) -> Vec<LoggedRequest> {
        wait_for_logged(&self.requests, matches).await
    }

    fn release(path: &Path) {
        std::fs::write(path, b"release").unwrap();
    }
}

mod lifecycle_events {
    use super::*;

    fn transitions(events: &EventLog) -> Vec<(SubagentStatus, Option<GenerationOutcome>)> {
        emitted(events)
            .into_iter()
            .filter_map(|event| match event {
                events::RuntimeEvent::SubagentStateChanged {
                    status, outcome, ..
                } => Some((status, outcome)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn nested_runtime_events_include_only_the_immediate_parent() {
        let root = tempfile::tempdir().unwrap();
        let base = manager_with_generic_harness(root.path(), Vec::new());
        let mut config = base.child_config();
        config.parent_id = Some("s-parent".into());
        config.parent_name = Some("偵察 🦀".into());
        let (manager, events) = observe_events(Subagents::new(config, 2));

        manager.insert_starting_for_test().await;

        assert!(matches!(
            emitted(&events).as_slice(),
            [events::RuntimeEvent::SubagentStateChanged {
                parent_id: Some(parent_id),
                parent_name: Some(parent_name),
                ..
            }] if parent_id == "s-parent" && parent_name == "偵察 🦀"
        ));
    }

    #[tokio::test]
    async fn emits_committed_create_prompt_failure_and_close_transitions() {
        let root = tempfile::tempdir().unwrap();
        let (manager, events) = observe_events(manager_with_generic_harness(
            root.path(),
            vec!["--no-fork".into()],
        ));
        let handle = manager
            .create(
                "base task".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            transitions(&events),
            vec![
                (SubagentStatus::Starting, None),
                (SubagentStatus::Working, None),
                (SubagentStatus::Idle, Some(GenerationOutcome::Success)),
            ]
        );

        let handle = manager
            .prompt(
                handle,
                "successful continuation".into(),
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            &transitions(&events)[3..],
            &[
                (SubagentStatus::Working, None),
                (SubagentStatus::Idle, Some(GenerationOutcome::Success)),
            ]
        );

        let error = manager
            .prompt(
                handle.clone(),
                "MOCK_REFUSAL".into(),
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "nested agent refused the prompt");
        manager
            .close(&handle.id, &TurnCancellation::default())
            .await
            .unwrap();

        let emitted = emitted(&events);
        assert_eq!(
            &transitions(&events)[5..],
            &[
                (SubagentStatus::Working, None),
                (SubagentStatus::Idle, Some(GenerationOutcome::Failed)),
                (SubagentStatus::Removed, Some(GenerationOutcome::Failed)),
            ]
        );
        assert!(emitted.iter().all(|event| event.parent_call().is_none()));
        let generations = emitted
            .iter()
            .filter_map(|event| match event {
                events::RuntimeEvent::SubagentStateChanged { generation, .. } => Some(*generation),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(generations, vec![1, 1, 1, 2, 2, 3, 3, 3]);
        assert!(
            matches!(emitted.last(), Some(events::RuntimeEvent::SubagentStateChanged { id, name, status: SubagentStatus::Removed, generation_finished_at_unix_ms: Some(_), .. }) if id.as_str() == handle.id.as_str() && Some(name.as_str()) == handle.name.as_deref())
        );
    }

    #[tokio::test]
    async fn failing_event_transport_does_not_change_subagent_operations() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = tempfile::tempdir().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let sink_attempts = Arc::clone(&attempts);
        let manager = manager_with_generic_harness(root.path(), vec!["--no-fork".into()])
            .with_event_sink_for_test(Arc::new(move |_| {
                sink_attempts.fetch_add(1, Ordering::Relaxed);
                Err(())
            }));

        let handle = manager
            .create(
                "create".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .expect("event failure must not fail create");
        let handle = manager
            .prompt(handle, "continue".into(), TurnCancellation::default(), None)
            .await
            .expect("event failure must not fail prompt");
        assert_eq!(
            manager.lookup(&handle).unwrap().lock().await.status,
            SubagentStatus::Idle
        );
        manager
            .close(&handle.id, &TurnCancellation::default())
            .await
            .expect("event failure must not fail close");
        assert!(manager.lookup(&handle).is_err());
        assert!(attempts.load(Ordering::Relaxed) >= 6);
    }

    #[tokio::test]
    async fn closed_sessions_emit_one_removed_transition_before_name_reuse() {
        for (working, expected_outcome) in [
            (false, Some(GenerationOutcome::Success)),
            (true, Some(GenerationOutcome::Failed)),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (manager, state, prior) = manager_with_disconnected_session(root.path());
            let (manager, events) = observe_events(manager);
            if working {
                let mut state = state.lock().await;
                state.status = SubagentStatus::Working;
                state.outcome = None;
                state.generation_finished_at_unix_ms = None;
            }

            manager.insert_starting_for_test().await;
            let removed = emitted(&events)
                    .into_iter()
                    .filter(|event| {
                        matches!(event, events::RuntimeEvent::SubagentStateChanged { id, status: SubagentStatus::Removed, .. } if id == &prior.id)
                    })
                    .collect::<Vec<_>>();
            assert_eq!(removed.len(), 1);
            assert!(matches!(
                &removed[0],
                events::RuntimeEvent::SubagentStateChanged { outcome, generation_finished_at_unix_ms: Some(_), .. } if *outcome == expected_outcome
            ));
            assert!(manager.lookup(&prior).is_err());
        }
    }

    #[tokio::test]
    async fn idle_child_exit_promptly_retires_the_direct_handle_once() {
        let root = tempfile::tempdir().unwrap();
        let (manager, events) = observe_events(manager_with_generic_harness(
            root.path(),
            vec!["--exit-after-prompt".into()],
        ));
        let handle = manager
            .create(
                "finish before exiting".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if transitions(&events).last()
                    == Some(&(SubagentStatus::Removed, Some(GenerationOutcome::Success)))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("idle child exit did not emit direct removal promptly");

        assert_eq!(
            transitions(&events),
            vec![
                (SubagentStatus::Starting, None),
                (SubagentStatus::Working, None),
                (SubagentStatus::Idle, Some(GenerationOutcome::Success)),
                (SubagentStatus::Removed, Some(GenerationOutcome::Success)),
            ]
        );
        assert!(manager.lookup(&handle).is_err());
        assert_eq!(manager.capacity.available_permits(), MAX_LIVE_SUBAGENTS);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            transitions(&events)
                .into_iter()
                .filter(|(status, _)| *status == SubagentStatus::Removed)
                .count(),
            1
        );
        manager.insert_starting_for_test().await;
    }

    #[tokio::test]
    async fn emits_removed_after_failed_creation_and_terminal_retirement() {
        let root = tempfile::tempdir().unwrap();
        let (failed, failed_events) = observe_events(manager_with_generic_harness(
            root.path(),
            vec!["--fail-start".into()],
        ));
        assert!(
            failed
                .create(
                    "create".into(),
                    CreateOptions::default(),
                    0,
                    TurnCancellation::default(),
                    None
                )
                .await
                .is_err()
        );
        assert_eq!(
            transitions(&failed_events),
            vec![
                (SubagentStatus::Starting, None),
                (SubagentStatus::Removed, Some(GenerationOutcome::Failed)),
            ]
        );

        let (terminal, _, prior) = manager_with_disconnected_session(root.path());
        let (terminal, terminal_events) = observe_events(terminal);
        assert!(
            terminal
                .prompt(prior, "continue".into(), TurnCancellation::default(), None)
                .await
                .is_err()
        );
        assert_eq!(
            transitions(&terminal_events),
            vec![
                (SubagentStatus::Working, None),
                (SubagentStatus::Removed, Some(GenerationOutcome::Failed)),
            ]
        );
    }
}

#[tokio::test]
async fn failed_create_and_fork_startup_record_failed_removed_transitions() {
    let root = tempfile::tempdir().unwrap();
    let (failed_create, create_events) = observe_events(manager_with_generic_harness(
        root.path(),
        vec!["--fail-start".into()],
    ));
    assert!(
        failed_create
            .create(
                "create".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .is_err()
    );
    let create_events = emitted(&create_events);
    assert!(matches!(
        create_events.last(),
        Some(events::RuntimeEvent::SubagentStateChanged {
            status: SubagentStatus::Removed,
            outcome: Some(GenerationOutcome::Failed),
            generation_finished_at_unix_ms: Some(_),
            ..
        })
    ));

    let (failed_fork, fork_events) = observe_events(manager_with_generic_harness(
        root.path(),
        vec!["--fail-fork".into()],
    ));
    let source = failed_fork
        .create(
            "source".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    assert!(
        failed_fork
            .fork(
                source,
                "fork".into(),
                None,
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .is_err()
    );
    let fork_events = emitted(&fork_events);
    assert!(matches!(
        fork_events.last(),
        Some(events::RuntimeEvent::SubagentStateChanged {
            status: SubagentStatus::Removed,
            outcome: Some(GenerationOutcome::Failed),
            generation_finished_at_unix_ms: Some(_),
            ..
        })
    ));
}

#[tokio::test]
async fn close_during_create_or_fork_prompt_cannot_publish_idle() {
    let create_scenario = MockAcpScenario::new(ScenarioOptions {
        gate_new: true,
        ..Default::default()
    });
    let create_manager = create_scenario.manager.clone();
    let create = tokio::spawn(async move {
        create_manager
            .create(
                "create".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
    });
    create_scenario
        .wait_for(|request| matches!(request, LoggedRequest::New { .. }))
        .await;
    let starting = create_scenario
        .manager
        .list(&TurnCancellation::default())
        .await
        .unwrap();
    assert_eq!(starting.len(), 1);
    assert_eq!(starting[0].status, SubagentStatus::Starting);
    create_scenario
        .manager
        .close(&starting[0].id, &TurnCancellation::default())
        .await
        .unwrap();
    MockAcpScenario::release(&create_scenario.new_release);
    assert!(create.await.unwrap().is_err());
    assert!(
        create_scenario
            .manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    wait_for_available_permits(&create_scenario.manager, MAX_LIVE_SUBAGENTS).await;

    let fork_scenario = MockAcpScenario::new(ScenarioOptions {
        gate_prompt: Some("branch"),
        ..Default::default()
    });
    let source = fork_scenario.create("source").await;
    let fork = fork_scenario.spawn_fork(source.clone(), "branch");
    fork_scenario
        .wait_for(
            |request| matches!(request, LoggedRequest::Prompt { text, .. } if text == "branch"),
        )
        .await;
    let branch = fork_scenario
        .manager
        .list(&TurnCancellation::default())
        .await
        .unwrap()
        .into_iter()
        .find(|listing| listing.id != source.id)
        .unwrap();
    assert_eq!(branch.status, SubagentStatus::Working);
    fork_scenario
        .manager
        .close(&branch.id, &TurnCancellation::default())
        .await
        .unwrap();
    MockAcpScenario::release(&fork_scenario.prompt_release);
    assert!(fork.await.unwrap().is_err());
    assert_eq!(
        fork_scenario
            .manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .iter()
            .map(listing_id)
            .collect::<Vec<_>>(),
        [source.id.as_str()]
    );
    fork_scenario
        .manager
        .close(&source.id, &TurnCancellation::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn native_fork_releases_the_source_before_the_branch_prompt() {
    let scenario = MockAcpScenario::new(ScenarioOptions {
        gate_fork: true,
        gate_prompt: Some("branch"),
        ..Default::default()
    });
    let source = scenario.create("source").await;
    let fork = scenario.spawn_fork(source.clone(), "branch");
    scenario
        .wait_for(|request| matches!(request, LoggedRequest::Fork { .. }))
        .await;

    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        scenario.manager.list(&TurnCancellation::default()),
    )
    .await
    .expect("list blocked behind native fork I/O")
    .unwrap();
    assert!(listed.iter().any(|listing| listing.id == source.id));
    assert!(
        listed.iter().any(|listing| {
            listing.id != source.id && listing.status == SubagentStatus::Starting
        })
    );

    let prompt_error = scenario
        .manager
        .prompt(
            source.clone(),
            "advance source".into(),
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(prompt_error.to_string(), "subagent session is being forked");
    let fork_error = scenario
        .manager
        .fork(
            source.clone(),
            "second branch".into(),
            None,
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(fork_error.to_string(), "subagent session is being forked");

    MockAcpScenario::release(&scenario.fork_release);
    scenario
        .wait_for(
            |request| matches!(request, LoggedRequest::Prompt { text, .. } if text == "branch"),
        )
        .await;
    let advanced = scenario
        .manager
        .prompt(
            source.clone(),
            "advance source".into(),
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(advanced.generation, source.generation + 1);
    MockAcpScenario::release(&scenario.prompt_release);
    let branch = fork.await.unwrap().unwrap();
    let requests = logged_requests(&scenario.requests);
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request, LoggedRequest::Fork { .. }))
            .count(),
        1
    );
    assert!(requests.iter().any(|request| {
        matches!(request, LoggedRequest::Prompt { text, .. } if text == "advance source")
    }));
    scenario
        .manager
        .close(&branch.id, &TurnCancellation::default())
        .await
        .unwrap();
    scenario
        .manager
        .close(&source.id, &TurnCancellation::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropped_fork_with_failed_close_holds_only_its_permit_until_process_exit() {
    let scenario = MockAcpScenario::new(ScenarioOptions {
        gate_prompt: Some("branch"),
        fail_close_session: Some("branch-1"),
        ..Default::default()
    });
    let source = scenario.create("source").await;
    let fork = scenario.spawn_fork(source.clone(), "branch");
    scenario
        .wait_for(|request| {
            matches!(
                request,
                LoggedRequest::Prompt { session_id, text }
                    if session_id == "branch-1" && text == "branch"
            )
        })
        .await;
    fork.abort();
    assert!(fork.await.unwrap_err().is_cancelled());
    MockAcpScenario::release(&scenario.prompt_release);
    scenario
        .wait_for(|request| {
            matches!(request, LoggedRequest::Close { session_id } if session_id == "branch-1")
        })
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let state = scenario.manager.lookup(&source).unwrap();
            if state.lock().await.forking.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fork reservation was not cleared");
    assert_eq!(
        scenario.manager.capacity.available_permits(),
        MAX_LIVE_SUBAGENTS - 2
    );
    assert_eq!(
        scenario
            .manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .iter()
            .map(listing_id)
            .collect::<Vec<_>>(),
        [source.id.as_str()]
    );

    scenario
        .manager
        .close(&source.id, &TurnCancellation::default())
        .await
        .unwrap();
    wait_for_available_permits(&scenario.manager, MAX_LIVE_SUBAGENTS).await;
}

#[tokio::test]
async fn successful_fork_handoff_cleans_up_if_receipt_is_not_acknowledged() {
    let scenario = MockAcpScenario::new(ScenarioOptions::default());
    let branch = scenario.create("branch").await;
    let (reply, response) = oneshot::channel();
    let manager = scenario.manager.clone();
    let cleanup_branch = branch.clone();
    let handoff = tokio::spawn(async move {
        manager.handoff_fork_success(reply, cleanup_branch).await;
    });

    let success = response.await.unwrap().unwrap();
    assert_eq!(success.value.id, branch.id);
    drop(success);
    handoff.await.unwrap();

    assert!(
        scenario
            .manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    wait_for_available_permits(&scenario.manager, MAX_LIVE_SUBAGENTS).await;
}

#[tokio::test]
async fn prompt_error_after_close_emits_no_ghost_idle_row() {
    let scenario = MockAcpScenario::new(ScenarioOptions {
        gate_prompt: Some("MOCK_REFUSAL"),
        ..Default::default()
    });
    let source = scenario.create("source").await;
    let prompt_manager = scenario.manager.clone();
    let prompt_source = source.clone();
    let prompt = tokio::spawn(async move {
        prompt_manager
            .prompt(
                prompt_source,
                "MOCK_REFUSAL".into(),
                TurnCancellation::default(),
                None,
            )
            .await
    });
    scenario
        .wait_for(|request| {
            matches!(request, LoggedRequest::Prompt { text, .. } if text == "MOCK_REFUSAL")
        })
        .await;
    scenario
        .manager
        .close(&source.id, &TurnCancellation::default())
        .await
        .unwrap();
    MockAcpScenario::release(&scenario.prompt_release);
    assert!(prompt.await.unwrap().is_err());
    assert!(
        scenario
            .manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn terminal_child_errors_do_not_depend_on_channel_close_timing() {
    let (child, _) = ChildSession::closure_probe_for_test();
    assert!(child_error_is_terminal(
        &ChildError::TerminalCancelled,
        &child
    ));
    assert!(child_error_is_terminal(
        &ChildError::TerminalFailed("transport ended".into()),
        &child
    ));
}

#[tokio::test]
async fn reusable_prompt_failure_remains_failed_idle_and_can_be_retried() {
    let root = tempfile::tempdir().unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
        "generic".into(),
        crate::acp_child::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture, "--no-fork".into()],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let manager = Subagents::new(
        ChildConfig {
            root: root.path().to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses,
            default_harness: "acp.generic".into(),
            parent_id: None,
            parent_name: None,
        },
        2,
    );
    let handle = manager
        .create(
            "base".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();

    let error = manager
        .prompt(
            handle.clone(),
            "MOCK_REFUSAL".into(),
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "nested agent refused the prompt");
    let state = manager
        .lookup(&handle)
        .expect("live child remains reusable");
    let state = state.lock().await;
    assert_eq!(state.status, SubagentStatus::Idle);
    assert_eq!(state.outcome, Some(GenerationOutcome::Failed));
    assert!(state.generation_finished_at_unix_ms.is_some());
    drop(state);

    let original_name = handle.name.clone();
    let retried = manager
        .prompt(handle, "retry".into(), TurnCancellation::default(), None)
        .await
        .expect("failed reusable generation does not stale the last handle");
    assert_eq!(retried.generation, 3);
    assert_eq!(retried.name, original_name);
}

#[tokio::test]
async fn listing_omits_terminally_retired_subagents() {
    let directory = tempfile::tempdir().unwrap();
    let (manager, _, prior) = manager_with_disconnected_session(directory.path());

    assert!(
        manager
            .prompt(
                prior.clone(),
                "continue".into(),
                TurnCancellation::default(),
                None,
            )
            .await
            .is_err()
    );
    assert!(manager.lookup(&prior).is_err());
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn listing_includes_named_starting_and_idle_subagents() {
    let root = tempfile::tempdir().unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
        "generic".into(),
        crate::acp_child::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture, "--no-fork".into()],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let manager = Subagents::new(
        ChildConfig {
            root: root.path().to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses,
            default_harness: "acp.generic".into(),
            parent_id: None,
            parent_name: None,
        },
        2,
    );
    let create_manager = manager.clone();
    let create = tokio::spawn(async move {
        create_manager
            .create(
                "first prompt".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
    });

    let listing = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(listing) = manager
                .list(&TurnCancellation::default())
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                break listing;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("starting subagent was not registered");
    let allocated_name = listing.name.clone();
    assert_eq!(listing.status, SubagentStatus::Starting);
    assert_eq!(listing.generation, 1);
    assert_eq!(listing.task, "first prompt");

    let completed = create.await.unwrap().unwrap();
    assert_eq!(completed.name.as_deref(), Some(allocated_name.as_str()));
    let listings = manager.list(&TurnCancellation::default()).await.unwrap();
    assert_eq!(listings[0].id, completed.id);
    assert_eq!(listings[0].name, allocated_name);
    assert_eq!(listings[0].status, SubagentStatus::Idle);
    assert_eq!(listings[0].generation, 1);
    assert_eq!(listings[0].task, "first prompt");
    assert_eq!(
        serde_json::to_value(&listings[0]).unwrap(),
        json!({
            "id": completed.id,
            "name": allocated_name.clone(),
            "status": "idle",
            "generation": 1,
            "task": "first prompt"
        })
    );

    let mut informational_name = completed.clone();
    informational_name.name = Some("Imposter".into());
    let prompt_manager = manager.clone();
    let continued = tokio::spawn(async move {
        prompt_manager
            .prompt(
                informational_name,
                "second prompt".into(),
                TurnCancellation::default(),
                None,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let listing = manager.list(&TurnCancellation::default()).await.unwrap();
            if listing[0].status == SubagentStatus::Working {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("working continuation was not listable");
    let continued = continued.await.unwrap().unwrap();
    assert_eq!(continued.name.as_deref(), Some(allocated_name.as_str()));
    let listing = manager.list(&TurnCancellation::default()).await.unwrap();
    assert_eq!(listing[0].generation, 2);
    assert_eq!(listing[0].task, "second prompt");
}

#[tokio::test]
async fn fork_uses_its_fresh_preferred_name() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_generic_harness(root.path(), Vec::new());
    let source = manager
        .create(
            "source".into(),
            CreateOptions {
                name: Some("Implementer".into()),
                ..Default::default()
            },
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    let source_name = source.name.clone();

    let fork = manager
        .fork(
            source,
            "branch".into(),
            Some("Reviewer".into()),
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(source_name.as_deref(), Some("Implementer"));
    assert_eq!(fork.name.as_deref(), Some("Reviewer"));
}

#[tokio::test]
async fn generic_harness_without_native_fork_returns_unsupported() {
    let root = tempfile::tempdir().unwrap();
    let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
    let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
        "generic".into(),
        crate::acp_child::AcpHarnessProfile {
            command: "python3".into(),
            args: vec![fixture, "--no-fork".into()],
            permissions: Default::default(),
        },
    )]))
    .unwrap();
    let manager = Subagents::new(
        ChildConfig {
            root: root.path().to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses,
            default_harness: "acp.generic".into(),
            parent_id: None,
            parent_name: None,
        },
        2,
    );
    let prior = manager
        .create(
            "base".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();

    let error = manager
        .fork(
            prior,
            "branch".into(),
            None,
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "ACP harness \"acp.generic\" does not advertise session/fork; transcript fallback is only available for Kit"
    );
}
