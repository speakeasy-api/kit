// Included only in ui's test module; exercises the real wire/app/render APIs.
use crate::runlet_progress::{
    Change as ProgressChange, Node as ProgressNode, Progress, State as ProgressState,
};

fn progress_wire(app: &mut App, event: RuntimeEvent) {
    let mut bytes = Vec::new();
    crate::events::write_event(&mut bytes, &event);
    let line = String::from_utf8(bytes).unwrap();
    app.apply(Update::Runtime(
        crate::events::parse(line.trim_end()).unwrap(),
    ));
}
fn source_event(
    owner: &str,
    incarnation: u64,
    sequence: u64,
    change: ProgressChange,
) -> RuntimeEvent {
    RuntimeEvent::RunletProgress {
        progress: Progress {
            owner: owner.into(),
            incarnation,
            sequence,
            change,
        },
    }
}
fn progress_start(app: &mut App, source: &str, incarnation: u64, healed: bool) {
    progress_wire(
        app,
        source_event(
            "call-1",
            incarnation,
            0,
            ProgressChange::Started {
                digest: crate::tui::progress::source_digest(source),
                healed,
            },
        ),
    );
}
fn progress_node(id: &str, state: ProgressState, call: bool) -> ProgressNode {
    ProgressNode {
        id: id.into(),
        call,
        start: 8,
        end: 34,
        state,
        attempt: 0,
    }
}
fn progress_step(app: &mut App, incarnation: u64, sequence: u64, node: ProgressNode) {
    progress_wire(
        app,
        source_event(
            "call-1",
            incarnation,
            sequence,
            ProgressChange::Step { node: Some(node) },
        ),
    );
}

#[test]
fn authoritative_progress_distinct_iterations_and_structural_evaluation() {
    let mut app = sample();
    progress_start(&mut app, SCRIPT, 1, false);
    progress_step(
        &mut app,
        1,
        1,
        progress_node("a", ProgressState::Succeeded, true),
    );
    let mut retry = progress_node("b", ProgressState::Running, true);
    retry.attempt = 1;
    progress_step(&mut app, 1, 2, retry.clone());
    progress_step(&mut app, 1, 2, retry); // duplicate
    progress_step(
        &mut app,
        1,
        3,
        progress_node("scope", ProgressState::Running, false),
    );
    let frame = render(&mut app, 140, 50);
    assert!(frame.contains("1 running"), "{frame}");
    assert!(frame.contains("1 succeeded"), "{frame}");
    assert!(!frame.contains("evaluating"), "{frame}");
    assert!(!frame.contains("resolved"));
    // Unknown descendant cannot mutate this call's observations or agent roster.
    progress_wire(
        &mut app,
        source_event(
            "descendant",
            99,
            0,
            ProgressChange::Started {
                digest: crate::tui::progress::source_digest(SCRIPT),
                healed: false,
            },
        ),
    );
    assert!(render(&mut app, 140, 50).contains("1 running"));
}

#[test]
fn authoritative_progress_gap_reorder_stale_and_replay_are_conservative() {
    for sequence in [0, 3] {
        let mut app = sample();
        progress_start(&mut app, SCRIPT, 2, false);
        progress_step(
            &mut app,
            2,
            1,
            progress_node("a", ProgressState::Running, true),
        );
        progress_step(
            &mut app,
            2,
            sequence,
            progress_node("a", ProgressState::Succeeded, true),
        );
        assert!(!render(&mut app, 140, 50).contains("# call @"));
        progress_start(&mut app, SCRIPT, 3, false);
        progress_step(
            &mut app,
            3,
            1,
            progress_node("new", ProgressState::Running, true),
        );
        assert!(render(&mut app, 140, 50).contains("1 running"));
        progress_step(
            &mut app,
            2,
            2,
            progress_node("old", ProgressState::Succeeded, true),
        );
        assert!(!render(&mut app, 140, 50).contains("# call @"));
    }
}

#[test]
fn authoritative_progress_healed_mismatch_and_unicode_spans_stay_neutral() {
    for (source, healed) in [(SCRIPT, true), ("return 1", false)] {
        let mut app = sample();
        progress_start(&mut app, source, 1, healed);
        progress_step(
            &mut app,
            1,
            1,
            progress_node("a", ProgressState::Running, true),
        );
        assert!(!render(&mut app, 140, 50).contains("# call @"));
    }
    let mut app = sample();
    let script = "return \"🦀\"";
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(script.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    progress_start(&mut app, script, 1, false);
    let mut node = progress_node("bad", ProgressState::Running, true);
    node.end = 9;
    progress_step(&mut app, 1, 1, node);
    assert!(!render(&mut app, 140, 50).contains("# call @"));
}

#[test]
fn authoritative_progress_incomplete_and_parent_completion_never_fabricate_success() {
    let mut app = sample();
    progress_start(&mut app, SCRIPT, 1, false);
    progress_step(
        &mut app,
        1,
        1,
        progress_node("a", ProgressState::Running, true),
    );
    progress_wire(
        &mut app,
        source_event("call-1", 1, 1, ProgressChange::Finished { complete: false }),
    );
    assert!(!render(&mut app, 140, 50).contains("# call @"));
    progress_start(&mut app, SCRIPT, 2, false);
    progress_step(
        &mut app,
        2,
        1,
        progress_node("a", ProgressState::Running, true),
    );
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
        script: None,
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    let call = app
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Tool(c) => Some(c),
            _ => None,
        })
        .unwrap();
    assert!(call.progress.labels(&call.script).is_empty());
}

#[test]
fn authoritative_progress_node_bound_invalidates_without_partial_success() {
    let mut app = sample();
    progress_start(&mut app, SCRIPT, 1, false);
    for i in 0..=crate::runlet_progress::MAX_NODES {
        progress_step(
            &mut app,
            1,
            i as u64 + 1,
            progress_node(&format!("node-{i}"), ProgressState::Succeeded, true),
        );
    }
    assert!(!render(&mut app, 140, 50).contains("# call @"));
}

async fn real_bridge_events(source: &str, capacity: usize, cancel: bool) -> Vec<RuntimeEvent> {
    real_bridge_events_with_transport(source, capacity, cancel, None).await
}

async fn real_bridge_events_with_transport(
    source: &str,
    capacity: usize,
    cancel: bool,
    transport: Option<crate::runlet_progress::transport::Transport>,
) -> Vec<RuntimeEvent> {
    use agentkit_core::{MetadataMap, SessionId, ToolCallId, TurnId};
    use agentkit_tool_compose::{
        BackendRun, ComposeBackend, ComposeConfig, ComposeOutcome, ComposeTool, RunletBackend,
        RunletProgress,
    };
    use agentkit_tools_core::{
        AllowAllPermissions, BasicToolExecutor, OwnedToolContext, ToolExecutionScope, ToolExecutor,
        ToolName, ToolRegistry, ToolRequest,
    };
    use std::{num::NonZeroUsize, sync::Arc};
    struct ObservedBackend(
        tokio::sync::mpsc::Sender<RunletProgress>,
        usize,
        Option<crate::runlet_progress::transport::Transport>,
    );
    #[async_trait::async_trait]
    impl ComposeBackend for ObservedBackend {
        fn name(&self) -> &'static str {
            RunletBackend.name()
        }
        fn description(&self, specs: Option<&[agentkit_tools_core::ToolSpec]>) -> String {
            RunletBackend.description(specs)
        }
        fn script_description(&self) -> &'static str {
            RunletBackend.script_description()
        }
        async fn execute(&self, run: BackendRun) -> Result<serde_json::Value, ComposeOutcome> {
            if let Some(transport) = &self.2 {
                return crate::runlet_progress::execute_observed(run, transport).await;
            }
            RunletBackend
                .execute_with_progress(run, self.0.clone(), NonZeroUsize::new(self.1).unwrap())
                .await
        }
    }
    struct Gate {
        spec: agentkit_tools_core::ToolSpec,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        finished: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl agentkit_tools_core::Tool for Gate {
        fn spec(&self) -> &agentkit_tools_core::ToolSpec {
            &self.spec
        }
        async fn invoke(
            &self,
            request: ToolRequest,
            _: &mut agentkit_tools_core::ToolContext<'_>,
        ) -> Result<agentkit_tools_core::ToolResult, agentkit_tools_core::ToolError> {
            self.entered.notify_one();
            self.release.notified().await;
            self.finished.notify_one();
            Ok(agentkit_tools_core::ToolResult::new(
                agentkit_core::ToolResultPart::success(
                    request.call_id,
                    agentkit_core::ToolOutput::Structured(serde_json::json!(1)),
                ),
            ))
        }
    }
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(tokio::sync::Notify::new());
    let gate = Gate {
        spec: agentkit_tools_core::ToolSpec::new(
            "progress_gate",
            "real host boundary",
            serde_json::json!({"type":"object"}),
        ),
        entered: entered.clone(),
        release: release.clone(),
        finished: finished.clone(),
    };
    let transported = transport.is_some();
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let compose = ComposeTool::new(ComposeConfig::default())
        .with_backend(ObservedBackend(tx, capacity, transport));
    let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::from_registry(
        ToolRegistry::new().with(compose).with(gate),
    ));
    let session_id = SessionId::new("session");
    let turn_id = TurnId::new("turn");
    let permissions = Arc::new(AllowAllPermissions);
    let resources: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
    let owned = OwnedToolContext {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        metadata: MetadataMap::new(),
        permissions: permissions.clone(),
        resources: resources.clone(),
        cancellation: None,
        approved_request: None,
        execution_scope: Some(ToolExecutionScope {
            executor: executor.clone(),
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            permissions,
            resources,
            cancellation: None,
        }),
    };
    {
        let mut context = owned.borrowed();
        let execute = executor.execute(
            ToolRequest {
                call_id: ToolCallId::new("call-1"),
                tool_name: ToolName::new("compose"),
                input: serde_json::json!({"script":source}),
                session_id,
                turn_id,
                metadata: MetadataMap::new(),
            },
            &mut context,
        );
        if cancel {
            use futures_util::future::{Either, select};
            let execute = std::pin::pin!(execute);
            let entered = std::pin::pin!(entered.notified());
            let result = select(execute, entered).await;
            assert!(
                matches!(result, Either::Right(_)),
                "host is held pending cancellation"
            );
        } else {
            let result = execute.await;
            assert!(
                matches!(
                    result,
                    agentkit_tools_core::ToolExecutionOutcome::Completed(_)
                ),
                "{result:?}"
            );
        }
    }
    if cancel {
        release.notify_one();
        finished.notified().await;
    }
    if transported {
        return Vec::new();
    }
    let mut stream = crate::runlet_progress::Active::new(
        rx.try_recv().unwrap(),
        crate::runlet_progress::transport::Transport::start(std::io::sink(), 16).unwrap(),
    );
    let mut events = Vec::new();
    while let Some(event) = stream.poll() {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn authoritative_progress_real_compose_bridge_to_diagnostic_app_and_render() {
    let source = "a = text.upper(\"é\")\nb = text.lower(a)\nreturn b";
    let events = real_bridge_events(source, 1024, false).await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::RunletProgress {
            progress: Progress {
                change: ProgressChange::Finished { complete: true },
                ..
            }
        }
    )));
    let mut app = sample();
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(source.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    for event in events {
        progress_wire(&mut app, event);
    }
    let frame = render(&mut app, 140, 50);
    assert!(frame.contains("succeeded"), "{frame}");
    assert!(!frame.contains("1 running"), "{frame}");
}

#[tokio::test]
async fn authoritative_progress_real_bridge_lag_invalidates_all_observations() {
    let source = "return text.upper(\"é\")";
    let events = real_bridge_events(source, 1, false).await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::RunletProgress {
            progress: Progress {
                change: ProgressChange::Finished { complete: false },
                ..
            }
        }
    )));
    let mut app = sample();
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(source.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    for event in events {
        progress_wire(&mut app, event);
    }
    assert!(!render(&mut app, 140, 50).contains("# call @"));
}

#[test]
fn authoritative_progress_two_subagents_dependency_is_not_descendant_completion() {
    let source = "implementation = subagent({prompt: \"implement\"})\nreview = subagent({prompt: implementation.output})\nreturn review";
    let mut app = sample();
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(source.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    progress_start(&mut app, source, 1, false);
    let implementation = ProgressNode {
        id: "implementation-attempt".into(),
        call: true,
        start: source.find("subagent").unwrap(),
        end: source.find('\n').unwrap(),
        state: ProgressState::Running,
        attempt: 0,
    };
    progress_step(&mut app, 1, 1, implementation);
    for descendant in [
        "child:compose:storage",
        "child:compose:tests",
        "child:compose:review",
    ] {
        progress_wire(
            &mut app,
            RuntimeEvent::ChildFinished {
                call: descendant.into(),
                tool: "subagent".into(),
                ok: true,
                summary: "done".into(),
                millis: 1,
            },
        );
    }
    let frame = render(&mut app, 150, 50);
    assert!(frame.contains("review = subagent"), "{frame}");
    let implementation_line = frame
        .lines()
        .find(|line| line.contains("implementation = subagent"))
        .unwrap();
    assert!(
        implementation_line.contains("# call @L1:C") && implementation_line.contains("1 running"),
        "{frame}"
    );
    let review_line = frame
        .lines()
        .find(|line| line.contains("review = subagent"))
        .unwrap();
    assert!(
        !review_line.contains("# call") && !review_line.contains("running"),
        "{frame}"
    );
    assert!(frame.contains("1 running"), "{frame}");
    assert!(!frame.contains("succeeded"), "{frame}");
    assert!(!frame.contains("blocked"), "{frame}"); // no event has established it
    let review = ProgressNode {
        id: "review-attempt".into(),
        call: true,
        start: source.rfind("subagent").unwrap(),
        end: source.rfind('\n').unwrap(),
        state: ProgressState::Blocked,
        attempt: 0,
    };
    progress_step(&mut app, 1, 2, review);
    assert!(render(&mut app, 150, 50).contains("1 blocked"));
}

#[test]
fn authoritative_progress_retained_run_bound_and_source_bound() {
    let mut app = sample();
    for i in 0..=crate::runlet_progress::MAX_RUNS {
        let owner = format!("owner-{i}");
        app.apply(Update::ToolStarted {
            id: owner.clone(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some(SCRIPT.into()),
            backgrounded: false,
        });
        progress_wire(
            &mut app,
            source_event(
                &owner,
                i as u64 + 1,
                0,
                ProgressChange::Started {
                    digest: crate::tui::progress::source_digest(SCRIPT),
                    healed: false,
                },
            ),
        );
        progress_wire(
            &mut app,
            source_event(
                &owner,
                i as u64 + 1,
                1,
                ProgressChange::Step {
                    node: Some(progress_node("n", ProgressState::Running, true)),
                },
            ),
        );
    }
    assert!(
        app.blocks
            .iter()
            .filter(|b| matches!(b, Block::Tool(c) if c.progress.retained()))
            .count()
            <= crate::runlet_progress::MAX_RUNS
    );
    let oldest = app
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Tool(c) if c.id == "owner-0" => Some(c),
            _ => None,
        })
        .unwrap();
    assert!(oldest.progress.labels(&oldest.script).is_empty());
    let huge = "🦀".repeat(crate::runlet_progress::MAX_SOURCE);
    app.apply(Update::ToolStarted {
        id: "huge".into(),
        title: "compose".into(),
        kind: ToolKind::Other,
        script: Some(huge),
        backgrounded: false,
    });
    let Block::Tool(last) = app.blocks.last().unwrap() else {
        panic!("tool")
    };
    assert!(last.script.len() <= crate::runlet_progress::MAX_SOURCE);
    assert!(last.script.ends_with("source truncated"));
}

#[tokio::test]
async fn authoritative_progress_real_bridge_cancellation_invalidates() {
    let source = "return progress_gate({})";
    let events = real_bridge_events(source, 1024, true).await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::RunletProgress {
            progress: Progress {
                change: ProgressChange::Finished { complete: false },
                ..
            }
        }
    )));
    let mut app = sample();
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(source.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    for event in events {
        progress_wire(&mut app, event);
    }
    assert!(!render(&mut app, 140, 50).contains("# call @"));
}

#[tokio::test]
async fn authoritative_progress_real_bridge_healing_stays_neutral() {
    let source = "if true { x = 1 }\nreturn 2";
    let events = real_bridge_events(source, 1024, false).await;
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::RunletProgress {
            progress: Progress {
                change: ProgressChange::Started { healed: true, .. },
                ..
            }
        }
    )));
    let mut app = sample();
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(source.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    for event in events {
        progress_wire(&mut app, event);
    }
    assert!(!render(&mut app, 140, 50).contains("# call @"));
}

#[tokio::test]
async fn authoritative_progress_real_iterations_remain_distinct() {
    let source = "values = for x in [\"a\", \"b\"] { return text.upper(x) }\nreturn values";
    let events = real_bridge_events(source, 1024, false).await;
    let mut app = sample();
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(source.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    for event in events {
        progress_wire(&mut app, event);
    }
    assert!(render(&mut app, 140, 50).contains("2 succeeded"));
}

#[test]
fn authoritative_progress_inline_nested_multiline_and_unicode_ranges() {
    let source =
        "first = text.upper(text.lower(\"🦀\"))\nsecond = text.upper(\n  first\n)\nreturn second";
    let mut app = sample();
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: None,
        script: Some(source.into()),
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    progress_start(&mut app, source, 1, false);
    let first_end = source.find('\n').unwrap();
    let outer = ProgressNode {
        id: "outer".into(),
        call: true,
        start: 8,
        end: first_end,
        state: ProgressState::Blocked,
        attempt: 0,
    };
    let inner = ProgressNode {
        id: "inner-old".into(),
        call: true,
        start: source.find("text.lower").unwrap(),
        end: first_end - 1,
        state: ProgressState::Failed,
        attempt: 0,
    };
    progress_step(&mut app, 1, 1, outer);
    progress_step(&mut app, 1, 2, inner.clone());
    progress_step(
        &mut app,
        1,
        3,
        ProgressNode {
            id: "inner-retry".into(),
            state: ProgressState::Running,
            attempt: 1,
            ..inner
        },
    );
    progress_step(
        &mut app,
        1,
        4,
        ProgressNode {
            id: "multiline".into(),
            call: true,
            start: source.rfind("text.upper").unwrap(),
            end: source.rfind("\nreturn").unwrap(),
            state: ProgressState::WaitingForCapacity,
            attempt: 0,
        },
    );
    progress_step(
        &mut app,
        1,
        5,
        ProgressNode {
            id: "root".into(),
            call: false,
            start: 0,
            end: source.len(),
            state: ProgressState::Running,
            attempt: 0,
        },
    );
    let frame = render(&mut app, 240, 50);
    let first = frame
        .lines()
        .find(|line| line.contains("first = text.upper"))
        .unwrap();
    assert!(
        first.contains("# call @L1:C9..L1:C36: 1 blocked"),
        "{frame}"
    );
    assert!(
        first.contains("# call @L1:C20..L1:C35: 1 running, 1 failed"),
        "{frame}"
    );
    let second = frame
        .lines()
        .find(|line| line.contains("second = text.upper"))
        .unwrap();
    assert!(
        second.contains("# call @L2:C10..L4:C2: 1 waiting for capacity"),
        "{frame}"
    );
    assert!(!frame.contains("bytes "), "{frame}");
    assert!(!frame.contains("evaluating"), "{frame}");
    assert!(
        !frame
            .lines()
            .find(|line| line.contains("return second"))
            .unwrap()
            .contains("# call")
    );
}

#[test]
fn authoritative_progress_conflicting_duplicates_fail_neutral() {
    for conflict in 0..5 {
        let mut app = sample();
        progress_start(&mut app, SCRIPT, 1, false);
        let node = progress_node("a", ProgressState::Succeeded, true);
        progress_step(&mut app, 1, 1, node.clone());
        progress_step(&mut app, 1, 1, node.clone());
        progress_start(&mut app, SCRIPT, 1, false);
        assert!(render(&mut app, 140, 50).contains("1 succeeded"));
        match conflict {
            0 => progress_step(
                &mut app,
                1,
                1,
                ProgressNode {
                    state: ProgressState::Failed,
                    ..node
                },
            ),
            1 => progress_start(&mut app, "return 1", 1, false),
            2 => progress_start(&mut app, SCRIPT, 1, true),
            3 => progress_wire(
                &mut app,
                source_event(
                    "call-1",
                    1,
                    2,
                    ProgressChange::Started {
                        digest: crate::tui::progress::source_digest(SCRIPT),
                        healed: false,
                    },
                ),
            ),
            _ => progress_step(
                &mut app,
                1,
                2,
                ProgressNode {
                    state: ProgressState::Failed,
                    ..node
                },
            ),
        }
        assert!(!render(&mut app, 140, 50).contains("# call @"));
        progress_start(&mut app, SCRIPT, 1, false);
        progress_step(
            &mut app,
            1,
            2,
            progress_node("a", ProgressState::Succeeded, true),
        );
        assert!(!render(&mut app, 140, 50).contains("# call @"));
    }
}

#[test]
fn authoritative_progress_reset_and_expired_lease_cannot_be_revived() {
    for explicit in [true, false] {
        let mut app = sample();
        progress_wire(&mut app, RuntimeEvent::RunletTransport { available: true });
        progress_start(&mut app, SCRIPT, 1, false);
        progress_step(
            &mut app,
            1,
            1,
            progress_node("a", ProgressState::Succeeded, true),
        );
        progress_wire(
            &mut app,
            source_event("call-1", 1, 1, ProgressChange::Finished { complete: true }),
        );
        assert!(render(&mut app, 140, 50).contains("1 succeeded"));
        if explicit {
            progress_wire(&mut app, RuntimeEvent::RunletTransport { available: false });
        } else {
            app.progress_tick_at(
                std::time::Instant::now() + crate::runlet_progress::transport::LEASE,
            );
        }
        assert!(!render(&mut app, 140, 50).contains("# call @"));
        progress_wire(&mut app, RuntimeEvent::RunletTransport { available: true });
        progress_start(&mut app, SCRIPT, 2, false);
        progress_step(
            &mut app,
            2,
            1,
            progress_node("b", ProgressState::Succeeded, true),
        );
        assert!(!render(&mut app, 140, 50).contains("# call @"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn authoritative_progress_production_publication_completes_and_cancels_with_stalled_reader() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
    };
    for cancel in [false, true] {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let mut filled = 0;
        loop {
            match writer.write(&[b'x'; 4096]) {
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("fill: {e}"),
            }
        }
        writer.set_nonblocking(false).unwrap();
        let transport = crate::runlet_progress::transport::Transport::start(writer, 2).unwrap();
        let source = if cancel {
            "return progress_gate({})"
        } else {
            "return text.upper(\"é\")"
        };
        // Deadlock watchdog, not a timing/performance assertion. Reader remains
        // completely stalled until the real production execute/drop finishes.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            real_bridge_events_with_transport(source, 1024, cancel, Some(transport.clone())),
        )
        .await
        .unwrap();
        drop(transport);
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut prefix = vec![0; filled];
        reader.read_exact(&mut prefix).unwrap();
        let mut wire = String::new();
        reader.read_to_string(&mut wire).unwrap();
        assert!(wire.lines().any(|line| matches!(
            crate::events::parse(line),
            Some(RuntimeEvent::RunletTransport { available: false })
        )));
    }
}

#[test]
fn authoritative_progress_terminal_conflicts_invalidate_completed_display() {
    let mut app = sample();
    progress_start(&mut app, SCRIPT, 1, false);
    progress_step(
        &mut app,
        1,
        1,
        progress_node("a", ProgressState::Succeeded, true),
    );
    progress_wire(
        &mut app,
        source_event("call-1", 1, 1, ProgressChange::Finished { complete: true }),
    );
    app.apply(Update::ToolPatched {
        id: "call-1".into(),
        title: None,
        kind: None,
        status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
        script: None,
        output: None,
        append_output: false,
        intent: None,
        backgrounded: false,
    });
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    progress_step(
        &mut app,
        1,
        1,
        progress_node("a", ProgressState::Succeeded, true),
    );
    progress_wire(
        &mut app,
        source_event("call-1", 1, 1, ProgressChange::Finished { complete: true }),
    );
    assert!(render(&mut app, 140, 50).contains("1 succeeded"));
    progress_wire(
        &mut app,
        source_event("call-1", 1, 1, ProgressChange::Finished { complete: false }),
    );
    assert!(!render(&mut app, 140, 50).contains("# call @"));
}
