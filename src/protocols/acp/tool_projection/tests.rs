use super::*;
use agentkit_acp::{AcpClientHandle, AcpClientMessage, SessionNotification};
use agentkit_core::{MetadataMap, SessionId, ToolCallId, TurnId};
use agentkit_tools_core::ToolName;
use serde_json::json;

pub(super) fn request(session: &str, name: &str, input: Value) -> ToolRequest {
    ToolRequest {
        session_id: SessionId::new(session),
        turn_id: TurnId::new("turn"),
        call_id: ToolCallId::new("parent:compose:node"),
        tool_name: ToolName::new(name),
        input,
        metadata: MetadataMap::new(),
    }
}

#[test]
fn evaluated_metadata_has_both_wire_shapes_without_retaining_arguments() {
    let registration = routes().register("metadata".into());
    let mut receiver = registration.buses().v1.subscribe();
    for (name, kind) in [
        ("shell", "execute"),
        ("edit", "edit"),
        ("read_file", "read"),
        ("tool_search", "search"),
        ("tool_schema", "read"),
        ("docs", "fetch"),
        ("prompt", "other"),
    ] {
        let request = request(
            "metadata",
            name,
            json!({"path": "image.png", "secret": "not projected"}),
        );
        let invocation = Invocation::start(&request, Some(Path::new("/workspace"))).unwrap();
        let update = loop {
            let update = receiver.try_recv().unwrap();
            if update.session == "metadata" && update.start.is_some() {
                break update;
            }
        };
        let v1 = serde_json::to_value(update.v1().unwrap()).unwrap();
        let v2 = serde_json::to_value(update.v2().unwrap()).unwrap();
        for value in [v1, v2] {
            assert_eq!(value["name"], name);
            assert_eq!(value["kind"].as_str().unwrap_or("other"), kind);
            assert_eq!(value["toolCallId"], "parent:compose:node");
            assert_eq!(value["_meta"]["kit/parentToolCallId"], "parent");
            assert!(!value.to_string().contains("not projected"));
            if matches!(name, "edit" | "read_file") {
                assert_eq!(value["locations"][0]["path"], "/workspace/image.png");
            }
        }
        invocation.finish(true);
    }
}

#[test]
fn oversized_and_non_compose_ids_are_not_projected() {
    let registration = routes().register("bounds".into());
    let _receiver = registration.buses().v1.subscribe();
    let mut request = request("bounds", "shell", json!({}));
    request.call_id = ToolCallId::new("x".repeat(MAX_ID + 1));
    assert!(Invocation::start(&request, None).is_none());
    request.call_id = ToolCallId::new("not-a-compose-child");
    assert!(Invocation::start(&request, None).is_none());
}

#[tokio::test]
async fn session_routing_and_drop_report_only_owned_calls() {
    let (client, mut messages) = AcpClientHandle::channel();
    let subscription = Subscription::start("routing".into(), move |update| {
        client
            .notify_session(SessionNotification::new(
                "wire-session",
                update.v1().unwrap(),
            ))
            .is_ok()
    });
    drop(Invocation::start(
        &request("other-session", "shell", json!({})),
        None,
    ));
    drop(Invocation::start(
        &request("routing", "shell", json!({})),
        None,
    ));
    subscription.drain().await.unwrap();
    drop(subscription);
    let first = messages.try_recv().unwrap();
    let second = messages.try_recv().unwrap();
    let AcpClientMessage::SessionNotification(first) = first else {
        panic!("expected notification")
    };
    let AcpClientMessage::SessionNotification(second) = second else {
        panic!("expected notification")
    };
    let first = serde_json::to_value(first).unwrap();
    let second = serde_json::to_value(second).unwrap();
    assert_eq!(first["sessionId"], "wire-session");
    assert_eq!(first["update"]["status"], "in_progress");
    assert_eq!(second["update"]["status"], "failed");
    assert!(messages.try_recv().is_err());
}

#[tokio::test]
async fn real_edit_wrapper_projects_success_and_failure_without_stderr_transport() {
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext, Tool};
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let tool = crate::tools::Observed::new(crate::tools::EditTool::new(root.path().into()))
        .with_root(root.path().into());
    let context = OwnedToolContext {
        session_id: SessionId::new("real-edit"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let registration = routes().register("real-edit".into());
    let mut receiver = registration.buses().v1.subscribe();
    for (input, ok) in [
        (
            json!({"op": "add", "path": "example.txt", "content": "private file contents"}),
            true,
        ),
        (
            json!({"op": "edit", "path": "example.txt", "hunks": [{"old": "private file contents", "new": "updated contents"}]}),
            true,
        ),
        (json!({"op": "delete", "path": "missing.txt"}), false),
    ] {
        let is_hunk = input["op"] == "edit";
        let result = tool
            .invoke(request("real-edit", "edit", input), &mut context.borrowed())
            .await;
        assert_eq!(result.is_ok(), ok);
        let updates = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter(|update| update.session == "real-edit")
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 2 + usize::from(is_hunk) + usize::from(ok));
        assert!(updates[0].start.is_some());
        assert_eq!(updates.last().unwrap().ok, ok);
        if ok {
            let update = &updates[updates.len() - 2];
            let v1 = serde_json::to_value(update.v1().unwrap()).unwrap();
            let v2 = serde_json::to_value(update.v2().unwrap()).unwrap();
            assert_eq!(v1["content"][0]["type"], "diff");
            assert_eq!(
                v1["content"][0]["path"],
                root.path().join("example.txt").to_str().unwrap()
            );
            assert_eq!(
                v1["content"][0]["newText"],
                if is_hunk {
                    "updated contents"
                } else {
                    "private file contents"
                }
            );
            assert_eq!(
                v1["content"][0]["oldText"],
                if is_hunk {
                    json!("private file contents")
                } else {
                    Value::Null
                }
            );
            assert_eq!(v2["content"][0]["type"], "diff");
            assert_eq!(
                v2["content"][0]["changes"][0]["operation"],
                if is_hunk { "modify" } else { "add" }
            );
            assert_eq!(
                v2["content"][0]["changes"][0]["path"],
                root.path().join("example.txt").to_str().unwrap()
            );
            assert!(v1["content"][0].get("changes").is_none());
            assert!(v2["content"][0].get("oldText").is_none());
        }
        if is_hunk {
            for wire in [
                updates[1].v1().map(|v| serde_json::to_value(v).unwrap()),
                updates[1].v2().map(|v| serde_json::to_value(v).unwrap()),
            ] {
                assert_eq!(wire.unwrap()["locations"][0]["line"], 1);
            }
        }
        assert!(
            !updates[0]
                .value()
                .to_string()
                .contains("private file contents")
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("example.txt")).unwrap(),
        "updated contents"
    );
}

#[tokio::test]
async fn drain_fails_when_the_client_has_closed() {
    let (client, messages) = AcpClientHandle::channel();
    drop(messages);
    let subscription = Subscription::start("closed-client".into(), move |update| {
        client
            .notify_session(SessionNotification::new("wire", update.v1().unwrap()))
            .is_ok()
    });
    drop(Invocation::start(
        &request("closed-client", "shell", json!({})),
        None,
    ));
    assert!(subscription.drain().await.is_err());
}

#[tokio::test]
async fn lag_invalidates_active_cards_and_recovers_for_fresh_calls() {
    let (sender, receiver) = broadcast::channel(2);
    let (client, mut messages) = AcpClientHandle::channel();
    let (_drains, commands) = tokio::sync::mpsc::channel(1);
    let task = tokio::spawn(forward(
        receiver,
        "lag".into(),
        move |update| {
            client
                .notify_session(SessionNotification::new(
                    "wire-session",
                    update.v1().unwrap(),
                ))
                .is_ok()
        },
        commands,
    ));
    let update = Update {
        session: "lag".into(),
        call: "parent:compose:node".into(),
        patch: None,
        ok: false,
        start: Some(
            json!({"toolCallId": "parent:compose:node", "title": "Running", "status": "in_progress"}),
        ),
    };
    sender.send(update.clone()).unwrap();
    let _ = messages.recv().await.unwrap();
    for _ in 0..3 {
        sender.send(update.clone()).unwrap();
    }
    let AcpClientMessage::SessionNotification(end) = messages.recv().await.unwrap() else {
        panic!("expected notification")
    };
    assert_eq!(
        serde_json::to_value(end).unwrap()["update"]["status"],
        "failed"
    );
    sender.send(update).unwrap();
    let AcpClientMessage::SessionNotification(fresh) = messages.recv().await.unwrap() else {
        panic!("expected notification")
    };
    assert_eq!(
        serde_json::to_value(fresh).unwrap()["update"]["status"],
        "in_progress"
    );
    drop(sender);
    let AcpClientMessage::SessionNotification(closed) = messages.recv().await.unwrap() else {
        panic!("expected notification")
    };
    assert_eq!(
        serde_json::to_value(closed).unwrap()["update"]["status"],
        "failed"
    );
    task.await.unwrap();
    assert!(messages.try_recv().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn real_shell_streams_bytes_before_exit_and_drains_terminal_before_call() {
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext, Tool};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let tool = crate::tools::Observed::new(crate::tools::ShellTool::new(root.path().into()));
    let context = OwnedToolContext {
        session_id: SessionId::new("real-terminal"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let subscription = Subscription::start_v2("real-terminal".into(), move |update| {
        send.send(update).is_ok()
    });
    let call = request(
        "real-terminal",
        "shell",
        json!({
            "command": "printf '\\377A'; while [ ! -f release ]; do sleep 0.01; done; printf 'err' >&2; exit 7",
            "timeout_seconds": 5,
        }),
    );
    let execution = tokio::spawn(async move { tool.invoke(call, &mut context.borrowed()).await });
    let mut bytes = Vec::new();
    let mut updates = Vec::new();
    // Release the actual process only after receiving a streamed byte, rather
    // than asserting a fragile wall-clock latency or accepting buffered output.
    while bytes.is_empty() {
        let update = tokio::time::timeout(std::time::Duration::from_secs(10), receive.recv())
            .await
            .unwrap()
            .unwrap();
        let value = serde_json::to_value(update.v2().unwrap()).unwrap();
        if value["sessionUpdate"] == "terminal_output_chunk" {
            bytes.extend(STANDARD.decode(value["data"].as_str().unwrap()).unwrap());
        }
        updates.push(update);
    }
    assert!(!execution.is_finished());
    std::fs::write(root.path().join("release"), "").unwrap();
    let result = execution.await.unwrap().unwrap();
    let agentkit_core::ToolOutput::Structured(result) = result.result.output else {
        panic!()
    };
    assert_eq!(result["exit_code"], 7);
    assert_eq!(result["stderr"], "err");
    subscription.drain().await.unwrap();
    while let Ok(update) = receive.try_recv() {
        let value = serde_json::to_value(update.v2().unwrap()).unwrap();
        if value["sessionUpdate"] == "terminal_output_chunk" {
            bytes.extend(STANDARD.decode(value["data"].as_str().unwrap()).unwrap());
        }
        updates.push(update);
    }
    assert_eq!(bytes, b"\xffAerr");
    let values: Vec<_> = updates
        .iter()
        .map(|u| serde_json::to_value(u.v2().unwrap()).unwrap())
        .collect();
    assert_eq!(values[1]["sessionUpdate"], "terminal_update");
    assert_eq!(values[2]["content"][0]["type"], "terminal");
    assert_eq!(values[2]["content"][0]["terminalId"], "parent:compose:node");
    assert_eq!(values[values.len() - 2]["exitStatus"]["exitCode"], 7);
    assert_eq!(values.last().unwrap()["status"], "completed");
    // v1 observes only the existing invocation lifecycle, no agent-owned terminal.
    assert_eq!(updates.iter().filter(|u| !u.v2_only()).count(), 2);
}

#[test]
fn terminal_drop_marks_exit_and_bounds_binary_chunks_and_metadata() {
    let registration = routes().register("terminal-bounds".into());
    let mut receiver = registration.buses().v2.subscribe();
    let request = request("terminal-bounds", "shell", json!({}));
    let invocation = Invocation::start(&request, None).unwrap();
    let terminal = terminal::Terminal::start(
        &request,
        &"x".repeat(terminal::MAX_COMMAND_BYTES + 1),
        Path::new("/tmp"),
    );
    terminal.output().unwrap().chunk(&vec![255; 20_000]);
    drop(terminal);
    invocation.finish(false);
    let updates: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
        .filter(|update| update.session == "terminal-bounds")
        .collect();
    let values: Vec<_> = updates
        .iter()
        .map(|u| serde_json::to_value(u.v2().unwrap()).unwrap())
        .collect();
    assert!(values[1].get("command").is_none());
    for value in &values {
        if value["sessionUpdate"] == "terminal_output_chunk" {
            assert!(value["data"].as_str().unwrap().len() <= 10_924);
        }
    }
    assert!(values[values.len() - 2]["exitStatus"].is_object());
    assert_eq!(values.last().unwrap()["status"], "failed");
}

#[tokio::test]
async fn lag_exits_live_terminal_before_invalidating_its_card() {
    let (sender, receiver) = broadcast::channel(2);
    let (_drain, commands) = tokio::sync::mpsc::channel(1);
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(forward(
        receiver,
        "terminal-lag".into(),
        move |u| send.send(u).is_ok(),
        commands,
    ));
    let start = Update {
        session: "terminal-lag".into(),
        call: "parent:compose:node".into(),
        start: Some(json!({"toolCallId": "parent:compose:node", "status": "in_progress"})),
        patch: None,
        ok: false,
    };
    sender.send(start.clone()).unwrap();
    receive.recv().await.unwrap();
    let mut terminal = start.clone();
    terminal.start = None;
    terminal.patch = Some(json!({"sessionUpdate": "terminal_update", "terminalId": terminal.call}));
    sender.send(terminal.clone()).unwrap();
    receive.recv().await.unwrap();
    // No await: force the bounded receiver to observe lag deterministically.
    for _ in 0..3 {
        sender.send(terminal.clone()).unwrap();
    }
    let exit = receive.recv().await.unwrap();
    let value = serde_json::to_value(exit.v2().unwrap()).unwrap();
    assert_eq!(value["sessionUpdate"], "terminal_update");
    assert!(value["exitStatus"].is_object());
    assert_eq!(value["_meta"]["kit/outputIncomplete"], true);
    let end = receive.recv().await.unwrap();
    assert_eq!(end.value()["status"], "failed");
    drop(sender);
    task.await.unwrap();
    assert!(receive.try_recv().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn real_shell_cancellation_and_timeout_exit_terminal_before_failed_card() {
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext, Tool};
    use std::sync::Arc;
    for cancel in [true, false] {
        let session = if cancel {
            "terminal-cancel"
        } else {
            "terminal-timeout"
        };
        let root = tempfile::tempdir().unwrap();
        let tool = crate::tools::Observed::new(crate::tools::ShellTool::new(root.path().into()));
        let controller = agentkit_core::CancellationController::new();
        let context = OwnedToolContext {
            session_id: SessionId::new(session),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: Some(controller.handle().checkpoint()),
            execution_scope: None,
            approved_request: None,
        };
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let subscription = Subscription::start_v2(session.into(), move |u| send.send(u).is_ok());
        let call = request(
            session,
            "shell",
            json!({"command": "printf ready; sleep 30", "timeout_seconds": 1}),
        );
        let execution =
            tokio::spawn(async move { tool.invoke(call, &mut context.borrowed()).await });
        loop {
            let update = tokio::time::timeout(std::time::Duration::from_secs(5), receive.recv())
                .await
                .unwrap()
                .unwrap();
            if update.value()["sessionUpdate"] == "terminal_output_chunk" {
                break;
            }
        }
        if cancel {
            controller.interrupt();
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), execution)
            .await
            .unwrap()
            .unwrap();
        if cancel {
            assert!(matches!(
                result,
                Err(agentkit_tools_core::ToolError::Cancelled)
            ));
        } else {
            assert!(
                matches!(result, Err(agentkit_tools_core::ToolError::ExecutionFailed(message)) if message.contains("timed out"))
            );
        }
        subscription.drain().await.unwrap();
        let values: Vec<_> = std::iter::from_fn(|| receive.try_recv().ok())
            .map(|u| serde_json::to_value(u.v2().unwrap()).unwrap())
            .collect();
        assert!(values[values.len() - 2]["exitStatus"].is_object());
        assert_eq!(values.last().unwrap()["status"], "failed");
    }
}

#[tokio::test]
async fn terminal_bursts_never_enter_the_v1_queue_with_mixed_clients() {
    let buses = Buses::new();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let (drains, commands) = tokio::sync::mpsc::channel(1);
    let subscription = Subscription {
        task: tokio::spawn(forward(
            buses.v1.subscribe(),
            "v1-isolated".into(),
            move |u| send.send(u).is_ok(),
            commands,
        )),
        drains,
    };
    let mut v2 = buses.v2.subscribe();
    let start = Update {
        session: "v1-isolated".into(),
        call: "parent:compose:node".into(),
        start: Some(json!({"toolCallId": "parent:compose:node", "status": "in_progress"})),
        patch: None,
        ok: false,
    };
    buses.publish(start.clone());
    for _ in 0..CAPACITY + 1 {
        buses.publish(Update {
            session: "v2-burst".into(), call: "parent:compose:other".into(),
            start: None, patch: Some(json!({"sessionUpdate": "terminal_output_chunk", "terminalId": "parent:compose:other", "data": "YQ=="})), ok: false,
        });
    }
    buses.publish(Update {
        start: None,
        ok: true,
        ..start
    });
    // The v2 receiver really did lag, while v1 had only lifecycle traffic.
    assert!(matches!(
        v2.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    subscription.drain().await.unwrap();
    let start = receive.try_recv().unwrap();
    let end = receive.try_recv().unwrap();
    assert!(start.start.is_some());
    assert_eq!(end.value()["status"], "completed");
    assert!(receive.try_recv().is_err());
}

#[tokio::test]
async fn cumulative_budget_bounds_a_stalled_transport_across_calls() {
    for size in [1, 8192] {
        // This external sink deliberately accepts without consumption, like the
        // SDK's unbounded queue. The budget must hold even without bus lag.
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let subscription =
            Subscription::start_v2("slow-terminal".into(), move |u| send.send(u).is_ok());
        for call in 0..4 {
            let mut request = request("slow-terminal", "shell", json!({}));
            request.call_id = ToolCallId::new(format!("parent:compose:{call}"));
            let invocation = Invocation::start(&request, None).unwrap();
            let terminal = terminal::Terminal::start(&request, "printf lots", Path::new("/tmp"));
            subscription.drain().await.unwrap();
            for _ in 0..64 {
                terminal.output().unwrap().chunk(&vec![255; size]);
                subscription.drain().await.unwrap();
            }
            terminal.finish(Some(0));
            invocation.finish(true);
            subscription.drain().await.unwrap();
        }
        let values: Vec<_> = std::iter::from_fn(|| receive.try_recv().ok())
            .map(|u| serde_json::to_value(u.v2().unwrap()).unwrap())
            .collect();
        let chunks: Vec<_> = values.iter().filter_map(|v| v["data"].as_str()).collect();
        assert!(!chunks.is_empty());
        assert!(chunks.len() <= 128);
        assert!(chunks.iter().map(|s| s.len()).sum::<usize>() <= 1024 * 1024);
        assert!(
            values
                .iter()
                .any(|v| v["_meta"]["kit/outputIncomplete"] == true)
        );
        assert_eq!(
            values.iter().filter(|v| v["status"] == "completed").count(),
            4
        );
        assert_eq!(
            values
                .iter()
                .filter(|v| v["exitStatus"]["exitCode"] == 0)
                .count(),
            4
        );
        assert!(!values.iter().any(|v| v["status"] == "failed"));
    }
}

impl Buses {
    fn publish(&self, update: Update) {
        if !update.v2_only() {
            let _ = self.v1.send(update.clone());
        }
        let _ = self.v2.send(update);
    }
}

#[tokio::test]
async fn unrelated_session_bursts_cannot_fail_active_calls() {
    for v2 in [false, true] {
        let session = format!("session-isolation-{v2}");
        let noisy = format!("session-noise-{v2}");
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let deliver = move |update| send.send(update).is_ok();
        let subscription = if v2 {
            Subscription::start_v2(session.clone(), deliver)
        } else {
            Subscription::start(session.clone(), deliver)
        };
        let noise = routes().register(noisy.clone());
        let mut noisy_receiver = if v2 {
            noise.buses().v2.subscribe()
        } else {
            noise.buses().v1.subscribe()
        };
        let invocation =
            Invocation::start(&request(&session, "subagent", json!({})), None).unwrap();
        subscription.drain().await.unwrap();
        assert!(receive.try_recv().unwrap().start.is_some());
        // No await: the active call's consumer cannot drain during the burst.
        // A process-global bounded queue would lose frames and fail this card.
        for _ in 0..CAPACITY + 1 {
            drop(Invocation::start(
                &request(&noisy, "shell", json!({})),
                None,
            ));
        }
        invocation.finish(true);
        assert!(matches!(
            noisy_receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        subscription.drain().await.unwrap();
        assert_eq!(receive.try_recv().unwrap().value()["status"], "completed");
        assert!(receive.try_recv().is_err());
    }
}

#[test]
fn route_cleanup_preserves_other_leases_and_recovers_from_poison() {
    let session = "route-cleanup";
    let first = routes().register(session.into());
    let second = routes().register(session.into());
    assert!(std::ptr::eq(first.buses(), second.buses()));
    drop(first);
    assert!(routes().senders(session).is_some());
    // Poison at a completed guarded transition, without changing its invariants.
    assert!(
        std::panic::catch_unwind(|| {
            let _guard = routes().lock();
            panic!("poison registry");
        })
        .is_err()
    );
    drop(second);
    assert!(routes().senders(session).is_none());
    let replacement = routes().register(session.into());
    assert!(routes().senders(session).is_some());
    drop(replacement);
    assert!(routes().senders(session).is_none());
}

#[tokio::test]
async fn route_leases_are_released_on_abort_and_delivery_unwind() {
    for unwind in [false, true] {
        let session = format!("route-task-cleanup-{unwind}");
        let mut subscription = Subscription::start(session.clone(), move |_| {
            panic!("delivery callback unwound");
        });
        if unwind {
            drop(Invocation::start(
                &request(&session, "shell", json!({})),
                None,
            ));
        } else {
            subscription.task.abort();
        }
        let error = (&mut subscription.task).await.unwrap_err();
        assert_eq!(error.is_panic(), unwind);
        assert!(routes().senders(&session).is_none());
    }
}

#[test]
fn concurrent_last_leases_remove_routes_despite_publisher_clones() {
    let session = "concurrent-route-cleanup";
    let first = routes().register(session.into());
    let second = routes().register(session.into());
    let senders = routes().senders(session).unwrap();
    let mut old_receiver = senders.0.subscribe();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for lease in [first, second] {
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                drop(lease);
            });
        }
    });
    assert!(routes().senders(session).is_none());
    let replacement = routes().register(session.into());
    let mut receiver = replacement.buses().v1.subscribe();
    drop(Invocation::start(
        &request(session, "shell", json!({})),
        None,
    ));
    assert!(receiver.try_recv().unwrap().start.is_some());
    assert!(old_receiver.try_recv().is_err());
    drop(replacement);
    assert!(routes().senders(session).is_none());
}

#[tokio::test]
async fn failed_delivery_releases_route() {
    let session = "failed-delivery-cleanup";
    let mut subscription = Subscription::start(session.into(), |_| false);
    drop(Invocation::start(
        &request(session, "shell", json!({})),
        None,
    ));
    (&mut subscription.task).await.unwrap();
    assert!(routes().senders(session).is_none());
}
