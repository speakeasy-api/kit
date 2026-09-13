//! Completion-time display of bounded child content on its existing parent card.
//! Raw child results remain unchanged; no child terminal command is executed.
use std::collections::{BTreeMap, BTreeSet};

use agentkit_tools_core::ToolRequest;
use serde_json::{Value, json};

use super::{CAPACITY, MAX_ID, Update, bus};

const MAX_ITEMS: usize = CAPACITY / 4;
const MAX_BYTES: usize = 64 * 1024;

pub(crate) fn publish(request: &ToolRequest, items: &[Value], truncated: bool) {
    if bus().receiver_count() == 0
        || request.session_id.0.len() > MAX_ID
        || request.call_id.0.len() > MAX_ID
        || !request.call_id.0.contains(":compose:")
    {
        return;
    }
    for patch in patches(request, items, truncated) {
        let _ = bus().send(Update {
            session: request.session_id.0.clone(),
            call: request.call_id.0.clone(),
            start: None,
            patch: Some(patch),
            ok: false,
        });
    }
}

fn patches(request: &ToolRequest, items: &[Value], truncated: bool) -> Vec<Value> {
    // Defend this boundary as well as ChildOutput's shared capture budget.
    if items.len() > MAX_ITEMS
        || items.iter().fold(0usize, |bytes, item| {
            bytes
                .saturating_add(serde_json::to_vec(item).map_or(MAX_BYTES + 1, |value| value.len()))
        }) > MAX_BYTES
    {
        return Vec::new();
    }
    let mut calls = BTreeMap::<&str, Vec<&Value>>::new();
    for item in items {
        let Some(id) = item["toolCallId"].as_str().filter(|id| id.len() <= MAX_ID) else {
            continue;
        };
        match item["sessionUpdate"].as_str() {
            Some("tool_call" | "tool_call_update") => {
                if let Some(content) = item.get("content") {
                    if content.is_null() {
                        calls.insert(id, Vec::new());
                    } else if let Some(content) = content.as_array() {
                        calls.insert(id, content.iter().collect());
                    }
                }
            }
            Some("tool_call_content_chunk") => {
                let content = calls.entry(id).or_default();
                content.push(&item["content"]);
            }
            _ => {}
        }
    }
    let mut content = Vec::new();
    let mut legacy = Vec::new();
    let mut terminals = BTreeMap::<&str, String>::new();
    let mut incomplete = truncated;
    for entry in calls.values().flatten() {
        if !matches!(entry["type"].as_str(), Some("diff" | "terminal")) {
            continue;
        }
        if content.len() >= MAX_ITEMS {
            incomplete = true;
            break;
        }
        match entry["type"].as_str() {
            Some("diff") => {
                if let Some(diff) = legacy_diff(entry) {
                    legacy.push(diff.clone());
                    content.push(diff);
                } else if let Ok(diff) =
                    serde_json::from_value::<agentkit_acp::v2::wire::Diff>((*entry).clone())
                    && !diff.changes.is_empty()
                    && let Ok(mut diff) = serde_json::to_value(diff)
                {
                    diff["type"] = Value::from("diff");
                    content.push(diff);
                }
            }
            Some("terminal") => {
                if let Some(id) = entry["terminalId"].as_str().filter(|id| id.len() <= MAX_ID) {
                    let mapped = terminal_id(request, id);
                    if terminals.insert(id, mapped.clone()).is_none() {
                        content.push(json!({"type": "terminal", "terminalId": mapped}));
                    }
                }
            }
            _ => {}
        }
    }
    if content.is_empty() {
        return Vec::new();
    }
    let mut patches = Vec::new();
    let mut exited = BTreeSet::new();
    // Explicit upserts make even a truncated capture's references well-defined.
    // A missing real exit is marked incomplete below, never invented.
    for mapped in terminals.values() {
        patches.push(json!({"sessionUpdate": "terminal_update", "terminalId": mapped}));
    }
    for item in items {
        let Some(id) = item["terminalId"].as_str() else {
            continue;
        };
        let Some(mapped) = terminals.get(id) else {
            continue;
        };
        let kind = item["sessionUpdate"].as_str();
        if !matches!(kind, Some("terminal_update" | "terminal_output_chunk")) {
            continue;
        }
        // Validate terminal wire fields; do not forward arbitrary child events.
        let Ok(update) =
            serde_json::from_value::<agentkit_acp::v2::wire::SessionUpdate>(item.clone())
        else {
            continue;
        };
        let Ok(mut value) = serde_json::to_value(update) else {
            continue;
        };
        value["terminalId"] = Value::from(mapped.clone());
        if kind == Some("terminal_update")
            && let Some(status) = value.get("exitStatus")
        {
            if status.is_object() {
                exited.insert(id);
            } else if status.is_null() {
                exited.remove(id);
            }
        }
        patches.push(value);
    }
    for (id, mapped) in terminals {
        if !exited.contains(id) || incomplete {
            patches.push(
                json!({"sessionUpdate": "terminal_update", "terminalId": mapped,
                "_meta": {"kit/outputIncomplete": true}}),
            );
        }
    }
    // Legacy file snapshots are representable on both protocols. Native v2 diffs
    // without old/new text cannot be truthfully turned into a v1 file snapshot.
    let metadata = if incomplete {
        Some(json!({"kit/outputIncomplete": true,
            "kit/parentToolCallId": request.call_id.0.rsplit_once(":compose:").map(|(owner, _)| owner)}))
    } else {
        None
    };
    if !legacy.is_empty() {
        let mut patch = json!({"toolCallId": request.call_id.0, "content": legacy});
        if let Some(metadata) = &metadata {
            patch["_meta"] = metadata.clone();
        }
        patches.push(patch);
    }
    let mut final_content = json!({"sessionUpdate": "tool_call_update", "toolCallId": request.call_id.0,
        "content": content});
    if let Some(metadata) = metadata {
        final_content["_meta"] = metadata;
    }
    patches.push(final_content);
    patches
}

fn legacy_diff(entry: &Value) -> Option<Value> {
    let diff: agent_client_protocol::schema::v1::Diff =
        serde_json::from_value(entry.clone()).ok()?;
    if !diff.path.is_absolute() || diff.path.as_os_str().len() > super::MAX_PATH {
        return None;
    }
    let path = diff.path.to_str()?;
    let operation = if diff.old_text.is_some() {
        "modify"
    } else {
        "add"
    };
    Some(
        json!({"type": "diff", "path": path, "oldText": diff.old_text,
        "newText": diff.new_text,
        "changes": [{"operation": operation, "path": path, "fileType": "text"}]}),
    )
}

fn terminal_id(request: &ToolRequest, id: &str) -> String {
    let mut hash = blake3::Hasher::new();
    for part in [&request.session_id.0, &request.call_id.0, id] {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("child-{}", hash.finalize().to_hex())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
