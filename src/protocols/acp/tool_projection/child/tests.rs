use super::*;

fn request(session: &str, name: &str, input: Value) -> ToolRequest {
    ToolRequest {
        session_id: agentkit_core::SessionId::new(session),
        turn_id: agentkit_core::TurnId::new("turn"),
        call_id: agentkit_core::ToolCallId::new("parent:compose:node"),
        tool_name: agentkit_tools_core::ToolName::new(name),
        input,
        metadata: agentkit_core::MetadataMap::new(),
    }
}

fn diff(path: &str) -> Value {
    json!({"type": "diff", "path": path, "oldText": "before", "newText": "after"})
}

#[test]
fn replacements_clears_and_chunks_form_one_final_snapshot() {
    let request = request("snapshots", "subagent", json!({}));
    let items = vec![
        json!({"sessionUpdate": "tool_call", "toolCallId": "a", "content": [diff("/old")]}),
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "content": [diff("/new")]}),
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed"}),
        json!({"sessionUpdate": "tool_call", "toolCallId": "b", "content": [diff("/cleared")]}),
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "b", "content": null}),
        json!({"sessionUpdate": "tool_call_content_chunk", "toolCallId": "b", "content": diff("/chunk")}),
    ];
    let patches = patches(&request, &items, false);
    let content = patches.last().unwrap()["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["path"], "/new");
    assert_eq!(content[1]["path"], "/chunk");
    assert_eq!(patches[0]["content"], patches[1]["content"]);
    assert_eq!(patches[0]["toolCallId"], request.call_id.0);
}

#[test]
fn native_v2_diffs_do_not_invent_v1_file_snapshots() {
    let request = request("native", "prompt", json!({}));
    let content = json!({"type": "diff", "changes": [{"operation": "modify", "path": "/file"}],
        "patch": {"format": "git_patch", "text": "-old\n+new\n"}});
    let items =
        [json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "content": [content]})];
    let patches = patches(&request, &items, false);
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0]["sessionUpdate"], "tool_call_update");
    let decoded: agentkit_acp::v2::wire::SessionUpdate =
        serde_json::from_value(patches[0].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap()["content"][0]["patch"]["text"],
        "-old\n+new\n"
    );
}

#[test]
fn terminal_replay_is_namespaced_ordered_and_does_not_change_raw_values() {
    let request = request("terminals", "fork", json!({}));
    let items = vec![
        json!({"sessionUpdate": "terminal_update", "terminalId": "t", "command": "echo hi"}),
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "content": [{"type": "terminal", "terminalId": "t"}]}),
        json!({"sessionUpdate": "terminal_output_chunk", "terminalId": "t", "data": "aGkK"}),
        json!({"sessionUpdate": "terminal_update", "terminalId": "t", "exitStatus": {"exitCode": 0}}),
    ];
    let original = items.clone();
    let patches = patches(&request, &items, false);
    assert_eq!(items, original);
    let id = terminal_id(&request, "t");
    assert_eq!(patches[0]["terminalId"], id);
    assert_eq!(patches[1]["command"], "echo hi");
    assert_eq!(patches[2]["data"], "aGkK");
    assert_eq!(patches[3]["exitStatus"]["exitCode"], 0);
    assert_eq!(patches.last().unwrap()["content"][0]["terminalId"], id);
    let mut other = request.clone();
    other.session_id = agentkit_core::SessionId::new("other");
    assert_ne!(terminal_id(&other, "t"), id);
    other = request.clone();
    other.call_id = agentkit_core::ToolCallId::new("parent:compose:next-generation");
    assert_ne!(terminal_id(&other, "t"), id);
    for patch in patches {
        let _: agentkit_acp::v2::wire::SessionUpdate = serde_json::from_value(patch).unwrap();
    }
}

#[test]
fn incomplete_terminal_capture_does_not_invent_process_exit() {
    let request = request("incomplete", "subagent", json!({}));
    let items = [
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "content": [
            {"type": "terminal", "terminalId": "missing"}
        ]}),
    ];
    let patches = patches(&request, &items, true);
    assert_eq!(patches[0]["sessionUpdate"], "terminal_update");
    assert!(patches[1].get("exitStatus").is_none());
    assert_eq!(patches[1]["_meta"]["kit/outputIncomplete"], true);
    assert!(patches[1]["exitStatus"].get("exitCode").is_none());
}

#[test]
fn capture_byte_accounting_excludes_array_overhead() {
    let request = request("exact-bound", "subagent", json!({}));
    let mut item = json!({"sessionUpdate": "tool_call_update", "toolCallId": "a",
        "content": [diff("/file")], "padding": ""});
    let padding = MAX_BYTES - serde_json::to_vec(&item).unwrap().len();
    item["padding"] = Value::from("x".repeat(padding));
    assert_eq!(serde_json::to_vec(&item).unwrap().len(), MAX_BYTES);
    assert!(!patches(&request, &[item], false).is_empty());
}

#[test]
fn display_limit_filters_plain_content_first_and_marks_rich_truncation() {
    let request = request("content-limit", "subagent", json!({}));
    let mut content = vec![
        json!({"type": "content", "content": {"type": "text", "text": "plain"}});
        MAX_ITEMS + 1
    ];
    content.push(diff("/after-plain"));
    let item = json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "content": content});
    let result = patches(&request, &[item], false);
    assert_eq!(result.last().unwrap()["content"][0]["path"], "/after-plain");
    let item = json!({"sessionUpdate": "tool_call_update", "toolCallId": "a",
        "content": vec![diff("/many"); MAX_ITEMS + 1]});
    let result = patches(&request, &[item], false);
    assert_eq!(
        result.last().unwrap()["content"].as_array().unwrap().len(),
        MAX_ITEMS
    );
    assert_eq!(
        result.last().unwrap()["_meta"]["kit/outputIncomplete"],
        true
    );
}

#[test]
fn cleared_terminal_exit_remains_unknown_after_replay() {
    let request = request("exit-clear", "subagent", json!({}));
    let items = [
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "content": [{"type": "terminal", "terminalId": "t"}]}),
        json!({"sessionUpdate": "terminal_update", "terminalId": "t", "exitStatus": {"exitCode": 0}}),
        json!({"sessionUpdate": "terminal_update", "terminalId": "t", "exitStatus": null}),
    ];
    let result = patches(&request, &items, false);
    let marker = &result[result.len() - 2];
    assert_eq!(marker["_meta"]["kit/outputIncomplete"], true);
    assert!(marker.get("exitStatus").is_none());
}

#[test]
fn count_and_byte_limits_reject_oversized_snapshots() {
    let request = request("bounds", "subagent", json!({}));
    assert!(patches(&request, &vec![json!({}); MAX_ITEMS + 1], false).is_empty());
    assert!(patches(&request, &[json!({"huge": "x".repeat(MAX_BYTES)})], false).is_empty());
}
