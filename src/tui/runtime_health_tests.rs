// Included in ui tests; exercises real diagnostic wire, app, and render APIs.
fn runtime_wire(app: &mut App, event: RuntimeEvent) {
    let mut bytes = Vec::new();
    crate::events::test_support::write_event(&mut bytes, &event);
    let line = String::from_utf8(bytes).unwrap();
    app.apply(Update::Runtime(
        crate::events::parse(line.trim_end()).unwrap(),
    ));
}
fn start_runtime_session(app: &mut App, session_id: &str) {
    app.start_session(session_id.into());
    app.apply(Update::ToolStarted {
        id: "call-1".into(),
        title: "compose".into(),
        kind: ToolKind::Other,
        script: Some(SCRIPT.into()),
        backgrounded: false,
    });
}

#[test]
fn runtime_loss_invalidates_all_runtime_lifecycle_state() {
    for explicit in [false, true] {
        let mut app = sample();
        start_runtime_session(&mut app, "session");
        runtime_wire(&mut app, RuntimeEvent::SessionStarted { session_id: "session".into() });
        let agent = RuntimeEvent::SubagentStateChanged {
            id: "child-agent".into(),
            name: "Child worker".into(),
            status: SubagentStatus::Working,
            outcome: None,
            generation: 1,
            task: "task".into(),
            parent_id: Some("parent-agent".into()),
            parent_name: Some("Parent".into()),
            harness: "acp.kit".into(),
            vendor: crate::events::HarnessVendor::Kit,
            model: None,
            created_at_unix_ms: 1,
            generation_started_at_unix_ms: 2,
            generation_finished_at_unix_ms: None,
        };
        runtime_wire(&mut app, agent.clone());
        runtime_wire(
            &mut app,
            RuntimeEvent::StorageStatus {
                pending: true,
                exhausted: true,
            },
        );
        runtime_wire(
            &mut app,
            RuntimeEvent::CompactionStarted {
                reason: "test".into(),
                at: 1,
            },
        );
        assert_eq!(app.agent_counts().working, 1);
        assert!(app.storage_pending && app.storage_exhausted && app.compacting);
        if explicit {
            runtime_wire(&mut app, RuntimeEvent::RunletTransport { available: false });
        } else {
            app.runtime_tick_at(
                std::time::Instant::now() + crate::diagnostic_transport::LEASE,
            );
        }
        // Actual completion/cleanup can be lost, delayed or followed by buffered
        // starts. None can make the incomplete stream trustworthy again.
        for event in [
            RuntimeEvent::SubagentDescendantsRemoved {
                ancestor_id: "parent-agent".into(),
            },
            RuntimeEvent::StorageStatus {
                pending: false,
                exhausted: false,
            },
            RuntimeEvent::CompactionFinished {
                reason: "test".into(),
                ok: true,
                compacted: true,
                millis: 2,
            },
            agent.clone(),
        ] {
            runtime_wire(&mut app, event);
        }
        assert!(app.runtime_unavailable());
        assert_eq!(app.agent_counts().total, 0);
        assert!(!app.storage_pending && !app.storage_exhausted && !app.compacting);
        let frame = render(&mut app, 140, 50);
        assert!(frame.contains("Runtime status unavailable"));
        assert!(frame.contains("storage state unknown"));
        assert!(!frame.contains("Child worker"));
        assert!(!frame.contains("compacting context"));
        assert!(!frame.contains("context compacted"));

        runtime_wire(&mut app, RuntimeEvent::RunletTransport { available: true });
        assert!(!app.runtime_unavailable());
        assert_eq!(app.agent_counts().total, 0);
        assert!(!app.storage_pending && !app.storage_exhausted && !app.compacting);
        // Leave room for the recovery note alongside the automatically opened roster.
        let frame = render(&mut app, 200, 50);
        assert!(!frame.contains("Runtime status unavailable"));
        assert!(frame.contains("Runtime status resumed"));
        assert!(frame.contains("state remains unknown"));
        assert!(!frame.contains("Child worker"));

        // Fresh observations are accepted without reviving cleared state.
        runtime_wire(&mut app, agent);
        runtime_wire(
            &mut app,
            RuntimeEvent::StorageStatus { pending: true, exhausted: false },
        );
        assert_eq!(app.agent_counts().working, 1);
        assert!(app.storage_pending);
        assert!(app.needs_redraw_tick());
        app.runtime_tick_at(std::time::Instant::now() + crate::diagnostic_transport::LEASE);
        assert!(app.runtime_unavailable());
        assert_eq!(app.agent_counts().total, 0);
        assert!(!app.storage_pending);
    }
}

#[test]
fn runtime_recovery_keeps_session_filtering_across_attachment_gaps() {
    for explicit in [false, true] {
        for attach_during_gap in [false, true] {
            let mut app = sample();
            start_runtime_session(&mut app, "old");
            runtime_wire(&mut app, RuntimeEvent::SessionStarted { session_id: "old".into() });
            if explicit {
                runtime_wire(&mut app, RuntimeEvent::RunletTransport { available: false });
            } else {
                app.runtime_tick_at(std::time::Instant::now() + crate::diagnostic_transport::LEASE);
            }
            start_runtime_session(&mut app, "new");
            if attach_during_gap {
                runtime_wire(&mut app, RuntimeEvent::SessionStarted { session_id: "new".into() });
            }
            let compaction = RuntimeEvent::CompactionStarted { reason: "test".into(), at: 1 };
            runtime_wire(&mut app, compaction.clone());
            assert!(app.runtime_unavailable());
            assert!(!app.compacting);
            runtime_wire(&mut app, RuntimeEvent::RunletTransport { available: true });
            runtime_wire(&mut app, compaction.clone());
            assert_eq!(app.compacting, attach_during_gap);
            // A heartbeat must not guess that the stream belongs to the selected session.
            if !attach_during_gap {
                runtime_wire(&mut app, RuntimeEvent::SessionStarted { session_id: "new".into() });
                runtime_wire(&mut app, compaction);
                assert!(app.compacting);
            }
        }
    }
}
