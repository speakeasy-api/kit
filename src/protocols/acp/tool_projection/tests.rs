use super::*;
use agentkit_acp::{AcpClientHandle, AcpClientMessage, SessionNotification};
use agentkit_core::{MetadataMap, SessionId, ToolCallId, TurnId};
use agentkit_tools_core::ToolName;
use serde_json::json;

fn request(session: &str, name: &str, input: Value) -> ToolRequest {
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
    let mut receiver = bus().subscribe();
    for (name, kind) in [
        ("shell", "execute"),
        ("edit", "edit"),
        ("read_file", "read"),
        ("tool_search", "search"),
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
    let _receiver = bus().subscribe();
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
    let mut receiver = bus().subscribe();
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
        assert_eq!(updates.len(), if is_hunk { 3 } else { 2 });
        assert!(updates[0].start.is_some());
        assert_eq!(updates.last().unwrap().ok, ok);
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
    let subscription = Subscription::start("real-terminal".into(), move |update| {
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
    let mut receiver = bus().subscribe();
    let request = request("terminal-bounds", "shell", json!({}));
    let invocation = Invocation::start(&request, None).unwrap();
    let terminal = terminal::Terminal::start(
        &request,
        &"x".repeat(crate::runlet_progress::MAX_SOURCE + 1),
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
        let subscription = Subscription::start(session.into(), move |u| send.send(u).is_ok());
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
