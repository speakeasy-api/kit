use super::super::*;
use serde_json::json;

fn update(patch: Value) -> Update {
    Update {
        session: "child-lifecycle".into(),
        call: "outer:compose:child".into(),
        start: None,
        patch: Some(patch),
        ok: false,
    }
}

#[test]
fn terminal_budget_markers_preserve_each_child_terminal_identity() {
    let mut budget = terminal::Budget::default();
    let mut first = terminal::State::running();
    for _ in 0..128 {
        let mut value = update(
            json!({"sessionUpdate": "terminal_output_chunk", "terminalId": "child-first", "data": "YQ=="}),
        );
        assert!(budget.admit(&mut value, &mut first));
        assert_eq!(value.value()["sessionUpdate"], "terminal_output_chunk");
    }
    for (id, state) in [
        ("child-first", &mut first),
        ("child-second", &mut terminal::State::running()),
    ] {
        let mut value = update(
            json!({"sessionUpdate": "terminal_output_chunk", "terminalId": id, "data": "YQ=="}),
        );
        assert!(budget.admit(&mut value, state));
        assert_eq!(value.value()["terminalId"], id);
        assert_eq!(value.value()["_meta"]["kit/outputIncomplete"], true);
        assert!(value.value().get("exitStatus").is_none());
    }
    for metadata in [Value::Null, json!({"child": "replacement"})] {
        let mut exit = update(
            json!({"sessionUpdate": "terminal_update", "terminalId": "child-first",
            "exitStatus": {"exitCode": 0}, "_meta": metadata}),
        );
        assert!(budget.admit(&mut exit, &mut first));
        assert_eq!(exit.value()["_meta"]["kit/outputIncomplete"], true);
        assert_eq!(exit.value()["exitStatus"]["exitCode"], 0);
    }
    let mut snapshot = update(
        json!({"sessionUpdate": "terminal_update", "terminalId": "child-third",
        "output": {"data": "YQ==", "truncated": false}, "exitStatus": {"exitCode": 0}}),
    );
    assert!(budget.admit(&mut snapshot, &mut terminal::State::running()));
    assert!(snapshot.value().get("output").is_none());
    assert_eq!(snapshot.value()["exitStatus"]["exitCode"], 0);
    assert_eq!(snapshot.value()["_meta"]["kit/outputIncomplete"], true);
}

#[test]
fn lag_marks_only_live_child_terminal_ids_without_fabricated_exits() {
    let (sender, mut receiver) = broadcast::channel(8);
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let sink = |update| send.send(update).is_ok();
    let mut active = HashMap::new();
    let mut budget = terminal::Budget::default();
    let mut start = update(Value::Null);
    start.patch = None;
    start.start = Some(json!({"toolCallId": start.call, "title": "Child"}));
    forward_event(
        Ok(start),
        &mut receiver,
        "child-lifecycle",
        &mut active,
        &mut budget,
        &sink,
    )
    .unwrap();
    for patch in [
        json!({"sessionUpdate": "terminal_update", "terminalId": "live"}),
        json!({"sessionUpdate": "terminal_update", "terminalId": "exited", "exitStatus": {"exitCode": 0}}),
        json!({"sessionUpdate": "terminal_update", "terminalId": "exited", "command": "metadata only"}),
    ] {
        forward_event(
            Ok(update(patch)),
            &mut receiver,
            "child-lifecycle",
            &mut active,
            &mut budget,
            &sink,
        )
        .unwrap();
    }
    while receive.try_recv().is_ok() {}
    for _ in 0..9 {
        sender.send(update(Value::Null)).unwrap();
    }
    let Err(broadcast::error::TryRecvError::Lagged(lost)) = receiver.try_recv() else {
        panic!("expected real channel lag");
    };
    forward_event(
        Err(broadcast::error::RecvError::Lagged(lost)),
        &mut receiver,
        "child-lifecycle",
        &mut active,
        &mut budget,
        &sink,
    )
    .unwrap();
    let marker = receive.try_recv().unwrap().value();
    assert_eq!(marker["terminalId"], "live");
    assert_eq!(marker["_meta"]["kit/outputIncomplete"], true);
    assert!(marker.get("exitStatus").is_none());
    assert_eq!(receive.try_recv().unwrap().value()["status"], "failed");
    assert!(receive.try_recv().is_err());
    assert!(active.is_empty());
}
