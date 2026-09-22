//! Completion-time display of bounded child content on its existing parent card.
//! Raw child results remain unchanged; no child terminal command is executed.
use std::collections::{BTreeMap, BTreeSet};

use agentkit_tools_core::ToolRequest;
use serde_json::{Map, Value};

use super::{CAPACITY, MAX_ID, Update, subscribers};

const MAX_ITEMS: usize = CAPACITY / 4;
const MAX_BYTES: usize = 64 * 1024;

pub(crate) fn publish(request: &ToolRequest, items: &[Value], truncated: bool) {
    if subscribers(&request.session_id.0) == 0
        || request.session_id.0.len() > MAX_ID
        || request.call_id.0.len() > MAX_ID
        || !request.call_id.0.contains(":compose:")
    {
        return;
    }
    for patch in patches(request, items, truncated) {
        super::publish(Update {
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
                let snapshot = legacy_diff(entry);
                // Dual-format content can carry a more precise native operation
                // (for example delete) and a patch. Do not replace it with the
                // legacy snapshot's inferred operation in the v2 projection.
                let native =
                    serde_json::from_value::<agentkit_acp::v2::wire::Diff>((*entry).clone())
                        .ok()
                        .filter(|diff| !diff.changes.is_empty())
                        .and_then(|diff| serde_json::to_value(diff).ok());
                if let Some(mut diff) = native {
                    diff["type"] = Value::from("diff");
                    content.push(diff);
                } else if let Some(diff) = &snapshot {
                    content.push(diff.clone());
                }
                if let Some(diff) = snapshot {
                    legacy.push(diff);
                }
            }
            Some("terminal") => {
                if let Some(id) = entry["terminalId"].as_str().filter(|id| id.len() <= MAX_ID) {
                    let mapped = terminal_id(request, id);
                    if terminals.insert(id, mapped.clone()).is_none() {
                        content.push(object([
                            ("type", Value::from("terminal")),
                            ("terminalId", Value::from(mapped)),
                        ]));
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
        patches.push(object([
            ("sessionUpdate", Value::from("terminal_update")),
            ("terminalId", Value::from(mapped.clone())),
        ]));
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
        // Keep only bounded display metadata. Raw child metadata remains in the
        // result, but must not bypass the terminal subscription's payload budget.
        let source_incomplete = value["_meta"]["kit/outputIncomplete"] == true;
        if let Some(object) = value.as_object_mut() {
            object.remove("_meta");
        }
        for field in ["output", "exitStatus"] {
            if let Some(object) = value.get_mut(field).and_then(Value::as_object_mut) {
                object.remove("_meta");
            }
        }
        let oversized_signal = value["exitStatus"]["signal"]
            .as_str()
            .is_some_and(|signal| signal.len() > 128);
        if oversized_signal
            && let Some(exit) = value.get_mut("exitStatus").and_then(Value::as_object_mut)
        {
            exit.remove("signal");
        }
        if source_incomplete || oversized_signal {
            value["_meta"] = object([("kit/outputIncomplete", Value::from(true))]);
        }
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
            patches.push(object([
                ("sessionUpdate", Value::from("terminal_update")),
                ("terminalId", Value::from(mapped)),
                (
                    "_meta",
                    object([("kit/outputIncomplete", Value::Bool(true))]),
                ),
            ]));
        }
    }
    // Legacy file snapshots are representable on both protocols. Native v2 diffs
    // without old/new text cannot be truthfully turned into a v1 file snapshot.
    let metadata = if incomplete {
        Some(object([
            ("kit/outputIncomplete", Value::Bool(true)),
            (
                "kit/parentToolCallId",
                Value::from(
                    request
                        .call_id
                        .0
                        .rsplit_once(":compose:")
                        .map(|(owner, _)| owner),
                ),
            ),
        ]))
    } else {
        None
    };
    if !legacy.is_empty() {
        let mut patch = object([
            ("toolCallId", Value::from(request.call_id.0.clone())),
            ("content", Value::Array(legacy)),
        ]);
        if let Some(metadata) = &metadata {
            patch["_meta"] = metadata.clone();
        }
        patches.push(patch);
    }
    let mut final_content = object([
        ("sessionUpdate", Value::from("tool_call_update")),
        ("toolCallId", Value::from(request.call_id.0.clone())),
        ("content", Value::Array(content)),
    ]);
    if let Some(metadata) = metadata {
        final_content["_meta"] = metadata;
    }
    patches.push(final_content);
    patches
}

fn legacy_diff(entry: &Value) -> Option<Value> {
    let diff: agent_client_protocol::schema::v1::Diff =
        serde_json::from_value(entry.clone()).ok()?;
    super::diff_content(&diff.path, diff.old_text.as_deref(), Some(&diff.new_text))
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(Map::from_iter(
        fields.into_iter().map(|(key, value)| (key.into(), value)),
    ))
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
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod integration_tests;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod lifecycle_tests;
