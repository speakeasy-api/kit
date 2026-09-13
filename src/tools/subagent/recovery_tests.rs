//! Nested under `tests` so the real ACP fixture helpers remain private.
use super::*;

#[tokio::test]
async fn dropped_detached_close_retains_capacity_until_deletion_finishes() {
    let mut scenario = RecoveryScenario::new(&["--delete"]);
    let prior = scenario.completed().await;
    let release = scenario.root.path().join("close-release");
    scenario
        .args
        .push(fixture_path_arg("--close-release", &release));
    let manager = scenario.manager(scenario.open(true));
    let closing = manager.clone();
    let task =
        tokio::spawn(async move { closing.close(&prior.id, &TurnCancellation::default()).await });
    wait_for_logged(&scenario.requests, |request| {
        matches!(request, LoggedRequest::Close { .. })
    })
    .await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(manager.capacity.available_permits(), MAX_LIVE_SUBAGENTS - 1);
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    std::fs::write(release, "continue").unwrap();
    wait_for_available_permits(&manager, MAX_LIVE_SUBAGENTS).await;
    assert_eq!(scenario.requests("session/delete").len(), 1);
    drop(manager);
    let manager = scenario.manager(scenario.open(true));
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
}

struct RecoveryScenario {
    root: tempfile::TempDir,
    storage: tempfile::TempDir,
    requests: PathBuf,
    args: Vec<String>,
}

impl RecoveryScenario {
    fn new(extra: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let requests = root.path().join("requests.jsonl");
        let mut args = vec!["--v2".into(), fixture_path_arg("--request-log", &requests)];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        Self {
            root,
            storage,
            requests,
            args,
        }
    }

    fn open(&self, resume: bool) -> session::OpenSession {
        session::open_in(
            self.root.path(),
            self.storage.path(),
            "recovery-parent",
            resume,
            false,
            if resume {
                Vec::new()
            } else {
                vec![agentkit_core::Item::text(
                    agentkit_core::ItemKind::System,
                    "parent",
                )]
            },
        )
        .unwrap()
    }

    fn manager(&self, opened: session::OpenSession) -> Subagents {
        manager_with_generic_harness(self.root.path(), self.args.clone())
            .with_observer(opened.observer, opened.children)
            .unwrap()
    }

    async fn completed(&self) -> SubagentValue {
        let manager = self.manager(self.open(false));
        let value = manager
            .create(
                "first turn".into(),
                CreateOptions::default(),
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(value.output, json!("first turn"));
        // Dropping the manager is shutdown, not explicit history deletion.
        drop(manager);
        value
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(&self.requests)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|request| request["method"] == method)
            .collect()
    }
}

async fn reprompt(
    manager: &Subagents,
    prior: SubagentValue,
    text: &str,
) -> Result<SubagentValue, ChildError> {
    manager
        .prompt(prior, text.into(), TurnCancellation::default(), None)
        .await
}

async fn assert_idle_handle(manager: &Subagents, prior: &SubagentValue) {
    let rows = manager.list(&TurnCancellation::default()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, prior.id);
    assert_eq!(rows[0].status, SubagentStatus::Idle);
    assert_eq!(rows[0].generation, prior.generation);
}

#[tokio::test]
async fn durable_identity_reopens_lazily_and_replay_does_not_leak_into_next_turn() {
    let scenario = RecoveryScenario::new(&[]);
    let prior = scenario.completed().await;
    let opened = scenario.open(true);
    assert_eq!(opened.transcript.len(), 1);
    assert_eq!(opened.children.len(), 1);
    let record = &opened.children[0];
    assert_eq!(record.id, prior.id);
    assert_ne!(record.id, record.acp_session_id);
    assert_eq!(record.acp_session_id, "base");
    assert_eq!(record.handle_generation, prior.generation);
    assert_eq!(record.output, prior.output);
    assert_eq!(record.lifecycle, session::ChildLifecycle::Idle);
    let manager = scenario.manager(opened);
    assert_idle_handle(&manager, &prior).await;
    assert!(scenario.requests("session/resume").is_empty());
    assert!(scenario.requests("session/delete").is_empty());

    let next = reprompt(&manager, prior.clone(), "next turn")
        .await
        .unwrap();
    assert_eq!(next.id, prior.id);
    assert_eq!(next.generation, prior.generation + 1);
    assert_eq!(next.output, json!("next turn"));
    let resumes = scenario.requests("session/resume");
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["sessionId"], "base");
    assert_eq!(scenario.requests("session/new").len(), 1);
    drop(manager);
    let reopened = scenario.open(true);
    assert_eq!(reopened.children[0].handle_generation, next.generation);
    assert_eq!(reopened.children[0].output, next.output);
}

#[tokio::test]
async fn stale_generation_is_rejected_before_detached_resume() {
    let scenario = RecoveryScenario::new(&[]);
    let prior = scenario.completed().await;
    let manager = scenario.manager(scenario.open(true));
    let mut stale = prior.clone();
    stale.generation += 1;
    let error = reprompt(&manager, stale, "must not run").await.unwrap_err();
    assert!(error.to_string().contains("stale subagent generation"));
    assert!(scenario.requests("session/resume").is_empty());
    assert_eq!(scenario.requests("session/prompt").len(), 1);
    assert_idle_handle(&manager, &prior).await;
    assert!(reprompt(&manager, prior, "valid").await.is_ok());
}

#[tokio::test]
async fn competing_prompts_only_advance_a_recovered_handle_once() {
    let scenario = RecoveryScenario::new(&[]);
    let prior = scenario.completed().await;
    let manager = scenario.manager(scenario.open(true));
    let (left, right) = tokio::join!(
        reprompt(&manager, prior.clone(), "left"),
        reprompt(&manager, prior.clone(), "right"),
    );
    let next = match (left, right) {
        (Ok(next), Err(_)) | (Err(_), Ok(next)) => next,
        other => panic!("exactly one prompt must succeed: {other:?}"),
    };
    assert_eq!(next.generation, prior.generation + 1);
    assert_eq!(scenario.requests("session/resume").len(), 1);
    assert_eq!(scenario.requests("session/prompt").len(), 2);
    assert_idle_handle(&manager, &next).await;
    assert!(reprompt(&manager, prior, "stale").await.is_err());
}

#[tokio::test]
async fn explicit_close_of_detached_child_persists_closed_and_deletes_history() {
    let scenario = RecoveryScenario::new(&["--delete"]);
    let prior = scenario.completed().await;
    let manager = scenario.manager(scenario.open(true));
    manager
        .close(&prior.id, &TurnCancellation::default())
        .await
        .unwrap();
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    for method in ["session/resume", "session/delete"] {
        let requests = scenario.requests(method);
        assert_eq!(requests.len(), 1, "{method}");
        assert_eq!(requests[0]["sessionId"], "base");
    }
    drop(manager);
    let opened = scenario.open(true);
    assert_eq!(
        opened.children[0].lifecycle,
        session::ChildLifecycle::Closed
    );
    let manager = scenario.manager(opened);
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(reprompt(&manager, prior, "cannot resurrect").await.is_err());
    assert_eq!(scenario.requests("session/resume").len(), 1);
}

#[tokio::test]
async fn interrupted_durable_child_is_filtered_on_restart() {
    let scenario = RecoveryScenario::new(&[]);
    let prior = scenario.completed().await;
    let opened = scenario.open(true);
    let mut record = opened.children[0].clone();
    // Exercise the durable boundary left by an interrupted turn, without
    // adding crash instrumentation to the production manager.
    record.lifecycle = session::ChildLifecycle::Interrupted;
    opened.observer.persist_child(&record).unwrap();
    drop(opened);
    let opened = scenario.open(true);
    assert_eq!(
        opened.children[0].lifecycle,
        session::ChildLifecycle::Interrupted
    );
    let manager = scenario.manager(opened);
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(reprompt(&manager, prior, "cannot resume").await.is_err());
    assert!(scenario.requests("session/resume").is_empty());
}

#[tokio::test]
async fn removed_harness_preserves_detached_handle_for_later_reconfiguration() {
    let scenario = RecoveryScenario::new(&[]);
    let prior = scenario.completed().await;
    let opened = scenario.open(true);
    let mut manager = manager_with_generic_harness(scenario.root.path(), scenario.args.clone());
    manager.config.harnesses = crate::acp_child::AcpHarnesses::new(Default::default()).unwrap();
    let manager = manager
        .with_observer(opened.observer, opened.children)
        .unwrap();
    let error = reprompt(&manager, prior.clone(), "missing harness")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no longer configured"));
    assert_idle_handle(&manager, &prior).await;
    assert!(scenario.requests("session/resume").is_empty());
    drop(manager);
    let manager = scenario.manager(scenario.open(true));
    assert!(reprompt(&manager, prior, "configured again").await.is_ok());
}

#[tokio::test]
async fn failed_resume_rolls_back_to_idle_and_retry_attempts_resume_again() {
    let scenario = RecoveryScenario::new(&[]);
    let prior = scenario.completed().await;
    let opened = scenario.open(true);
    let mut record = opened.children[0].clone();
    // The external fixture selects a real resume RPC failure by ACP identity.
    record.acp_session_id = "replay-failure".into();
    opened.observer.persist_child(&record).unwrap();
    drop(opened);
    let manager = scenario.manager(scenario.open(true));
    for _ in 0..2 {
        let error = reprompt(&manager, prior.clone(), "retry")
            .await
            .unwrap_err();
        assert!(matches!(error, ChildError::Failed(_)), "{error}");
        assert_idle_handle(&manager, &prior).await;
    }
    assert_eq!(scenario.requests("session/resume").len(), 2);
    assert_eq!(scenario.requests("session/prompt").len(), 1);
    drop(manager);
    let opened = scenario.open(true);
    assert_eq!(opened.children[0].lifecycle, session::ChildLifecycle::Idle);
    assert_eq!(opened.children[0].handle_generation, prior.generation);
    assert_eq!(opened.children[0].output, prior.output);
}

#[tokio::test]
async fn recovered_generic_harness_does_not_gain_transcript_fork_fallback() {
    let scenario = RecoveryScenario::new(&["--no-fork"]);
    let prior = scenario.completed().await;
    let manager = scenario.manager(scenario.open(true));
    let error = manager
        .fork(
            prior.clone(),
            "fork".into(),
            None,
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("transcript fallback is only available for Kit")
    );
    assert!(scenario.requests("session/fork").is_empty());
    assert_eq!(scenario.requests("session/new").len(), 1);
    assert_eq!(scenario.requests("session/prompt").len(), 1);
    assert_idle_handle(&manager, &prior).await;
    let next = reprompt(&manager, prior, "still usable").await.unwrap();
    assert_eq!(next.output, json!("still usable"));
}

#[tokio::test]
async fn close_during_reconnect_rejects_without_tombstone_or_second_resume() {
    let scenario = RecoveryScenario::new(&["--delete"]);
    let prior = scenario.completed().await;
    let opened = scenario.open(true);
    let mut record = opened.children[0].clone();
    record.acp_session_id = "replay-stall".into();
    opened.observer.persist_child(&record).unwrap();
    drop(opened);
    let manager = scenario.manager(scenario.open(true));
    let controller = agentkit_core::CancellationController::new();
    let cancellation = controller.handle().checkpoint();
    let task_manager = manager.clone();
    let task_prior = prior.clone();
    let prompt = tokio::spawn(async move {
        task_manager
            .prompt(task_prior, "waiting".into(), cancellation, None)
            .await
    });
    let marker = scenario.root.path().join("replay-stalled");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture must enter replay before close");
    let error = manager
        .close(&prior.id, &TurnCancellation::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("reconnect is still starting"));
    let rows = manager.list(&TurnCancellation::default()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, prior.id);
    assert_eq!(rows[0].status, SubagentStatus::Starting);
    assert_eq!(scenario.requests("session/resume").len(), 1);
    assert!(scenario.requests("session/delete").is_empty());
    controller.interrupt();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), prompt)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(ChildError::Cancelled)));
    assert_idle_handle(&manager, &prior).await;
    drop(manager);
    let opened = scenario.open(true);
    assert_eq!(opened.children, vec![record]);
    let manager = scenario.manager(opened);
    assert_idle_handle(&manager, &prior).await;
    assert_eq!(scenario.requests("session/resume").len(), 1);
}

#[tokio::test]
async fn tightened_model_policy_rejects_reconnect_before_launch() {
    let scenario = RecoveryScenario::new(&["--models"]);
    let manager = scenario.manager(scenario.open(false));
    let prior = manager
        .create(
            "first turn".into(),
            CreateOptions {
                model: Some("mock/requested".into()),
                ..Default::default()
            },
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    drop(manager);
    let opened = scenario.open(true);
    assert_eq!(opened.children[0].model.as_deref(), Some("mock/requested"));
    let mut manager = manager_with_generic_harness(scenario.root.path(), scenario.args.clone());
    manager.config.harnesses = manager
        .config
        .harnesses
        .clone()
        .with_model_policies(std::collections::BTreeMap::from([(
            "acp.generic".into(),
            crate::acp_child::SubagentHarnessPolicy {
                allow_model_overrides: Some(vec!["mock/default".into()]),
                ..Default::default()
            },
        )]))
        .unwrap();
    let manager = manager
        .with_observer(opened.observer, opened.children)
        .unwrap();
    let error = reprompt(&manager, prior.clone(), "blocked")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is not allowed"));
    assert_idle_handle(&manager, &prior).await;
    assert_eq!(scenario.requests("initialize").len(), 1);
    assert!(scenario.requests("session/resume").is_empty());
    drop(manager);
    let manager = scenario.manager(scenario.open(true));
    assert!(reprompt(&manager, prior, "allowed again").await.is_ok());
}

#[tokio::test]
async fn abandoned_completed_fork_is_tombstoned_and_not_restored() {
    let scenario = RecoveryScenario::new(&[]);
    let prior = scenario.completed().await;
    let manager = scenario.manager(scenario.open(true));
    let branch = manager
        .fork(
            prior.clone(),
            "branch turn".into(),
            None,
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    assert_ne!(branch.id, prior.id);
    assert_eq!(branch.output, json!("branch turn"));
    assert_eq!(scenario.requests("session/fork").len(), 1);
    // This is the cleanup boundary used when delivery of a successful handoff fails.
    manager.cleanup_abandoned_fork(&branch).await;
    let rows = manager.list(&TurnCancellation::default()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, prior.id);
    drop(manager);
    let opened = scenario.open(true);
    let record = opened
        .children
        .iter()
        .find(|record| record.id == branch.id)
        .unwrap();
    assert_eq!(record.lifecycle, session::ChildLifecycle::Closed);
    let manager = scenario.manager(opened);
    let rows = manager.list(&TurnCancellation::default()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, prior.id);
    let resumes = scenario.requests("session/resume").len();
    assert!(
        reprompt(&manager, branch, "cannot resurrect")
            .await
            .is_err()
    );
    assert_eq!(scenario.requests("session/resume").len(), resumes);
}

#[tokio::test]
async fn v1_recovery_loads_history_without_leaking_replay_into_output() {
    let mut scenario = RecoveryScenario::new(&["--load"]);
    scenario.args.retain(|arg| arg != "--v2");
    let prior = scenario.completed().await;
    let manager = scenario.manager(scenario.open(true));
    assert_idle_handle(&manager, &prior).await;
    assert!(scenario.requests("session/load").is_empty());
    let next = reprompt(&manager, prior.clone(), "fresh v1 turn")
        .await
        .unwrap();
    assert_eq!(next.id, prior.id);
    assert_eq!(next.generation, prior.generation + 1);
    assert_eq!(next.output, json!("fresh v1 turn"));
    let loads = scenario.requests("session/load");
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0]["sessionId"], "base");
    assert_eq!(scenario.requests("session/new").len(), 1);
    assert!(scenario.requests("session/resume").is_empty());
}

#[tokio::test]
async fn v1_without_load_rejects_recovery_without_creating_replacement_session() {
    let mut scenario = RecoveryScenario::new(&[]);
    scenario.args.retain(|arg| arg != "--v2");
    let prior = scenario.completed().await;
    let manager = scenario.manager(scenario.open(true));
    let error = reprompt(&manager, prior.clone(), "cannot load")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not advertise session/load")
    );
    assert_idle_handle(&manager, &prior).await;
    assert_eq!(scenario.requests("session/new").len(), 1);
    assert_eq!(scenario.requests("session/prompt").len(), 1);
    assert!(scenario.requests("session/load").is_empty());
    assert!(scenario.requests("session/resume").is_empty());
}

#[tokio::test]
async fn with_observer_rejects_nonempty_registry() {
    let scenario = RecoveryScenario::new(&[]);
    let manager = manager_with_generic_harness(scenario.root.path(), scenario.args.clone());
    let prior = manager
        .create(
            "live child".into(),
            CreateOptions::default(),
            0,
            TurnCancellation::default(),
            None,
        )
        .await
        .unwrap();
    assert_idle_handle(&manager, &prior).await;
    let opened = scenario.open(false);
    let error = manager
        .with_observer(opened.observer, opened.children)
        .err()
        .unwrap();
    assert!(error.contains("fresh unshared subagent manager"));
}

#[tokio::test]
async fn with_observer_rejects_shared_registry_without_changing_other_owner() {
    let scenario = RecoveryScenario::new(&[]);
    let manager = manager_with_generic_harness(scenario.root.path(), scenario.args.clone());
    let other_owner = manager.clone();
    let opened = scenario.open(false);
    let error = manager
        .with_observer(opened.observer, opened.children)
        .err()
        .unwrap();
    assert!(error.contains("fresh unshared subagent manager"));
    assert!(
        other_owner
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    // Once the failed attachment releases its owner, this fresh registry is usable.
    let opened = scenario.open(true);
    let manager = other_owner
        .with_observer(opened.observer, opened.children)
        .unwrap();
    assert!(
        manager
            .list(&TurnCancellation::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(scenario.requests("initialize").is_empty());
}
