//! Lossless native-to-canonical projection. Only native binary parts are attachments;
//! structured values stay inline and never become generic envelope attachments.
use agenthooksprotocol::Attachment;
use agentkit_core::{DataRef, Item, ItemKind, Part, ToolOutput};
use serde_json::Value;
use std::collections::BTreeSet;

pub(super) fn text_parts(value: &str) -> Value {
    Value::Array(vec![text_part("kit-text-0", value)])
}

pub(super) fn restore_text(parts: &Value) -> Result<String, String> {
    let parts = parts.as_array().ok_or("expected canonical parts array")?;
    let mut text = String::new();
    for part in parts {
        text.push_str(inline_text(part)?);
    }
    Ok(text)
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn text_part(id: &str, text: &str) -> Value {
    object([
        ("id", Value::String(id.to_owned())),
        ("kind", Value::String("text".to_owned())),
        ("mediaType", Value::String("text/plain".to_owned())),
        ("selection", Value::String("body".to_owned())),
        ("text", Value::String(text.to_owned())),
    ])
}

fn inline_text(part: &Value) -> Result<&str, String> {
    if part["kind"] != "text" || part["mediaType"] != "text/plain" || part["selection"] != "body" {
        return Err("expected inline text/plain canonical part".into());
    }
    part["text"]
        .as_str()
        .ok_or_else(|| "missing canonical text".into())
}

enum Shape {
    Array,
    Item,
    Output(ToolOutput),
}

fn snapshot(value: &Value, role: &str) -> Result<(Shape, Vec<Item>), String> {
    if value.is_array() {
        return serde_json::from_value(value.clone())
            .map(|items| (Shape::Array, items))
            .map_err(|e| e.to_string());
    }
    if let Ok(item) = serde_json::from_value::<Item>(value.clone()) {
        return Ok((Shape::Item, vec![item]));
    }
    let output: ToolOutput = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let parts = match &output {
        ToolOutput::Text(text) => vec![Part::text(text.clone())],
        ToolOutput::Structured(value) => vec![Part::structured(value.clone())],
        ToolOutput::Parts(parts) => parts.clone(),
        ToolOutput::Files(files) => files.iter().cloned().map(Part::File).collect(),
    };
    Ok((
        Shape::Output(output),
        vec![Item::new(native_role(role)?, parts)],
    ))
}

fn native_role(role: &str) -> Result<ItemKind, String> {
    match role {
        "system" => Ok(ItemKind::System),
        "developer" => Ok(ItemKind::Developer),
        "user" => Ok(ItemKind::User),
        "assistant" => Ok(ItemKind::Assistant),
        "tool" => Ok(ItemKind::Tool),
        _ => Err(format!("unsupported native role {role}")),
    }
}

fn role(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::System => "system",
        ItemKind::Developer => "developer",
        ItemKind::User => "user",
        ItemKind::Assistant => "assistant",
        ItemKind::Tool => "tool",
        ItemKind::Context => "system",
        ItemKind::Notification => "user",
    }
}

// Keep provider identifiers verbatim; synthesize collision-free identifiers only
// for anonymous items. Restore computes these against the original snapshot.
fn message_ids(items: &[Item]) -> Result<Vec<String>, String> {
    let mut used = BTreeSet::new();
    for item in items {
        if let Some(id) = &item.id
            && !used.insert(id.0.clone())
        {
            return Err("duplicate native message id".into());
        }
    }
    let mut ids = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if let Some(id) = &item.id {
            ids.push(id.0.clone());
        } else {
            let mut id = format!("kit-message-{index}");
            while !used.insert(id.clone()) {
                id.push('-');
            }
            ids.push(id);
        }
    }
    Ok(ids)
}
fn part_id(message_id: &str, index: usize) -> String {
    format!("{message_id}/part/{index}")
}

fn json_part(id: &str, value: &Value) -> Result<Value, String> {
    let mut part = text_part(
        id,
        &serde_json::to_string(value).map_err(|e| e.to_string())?,
    );
    part["category"] = Value::String("native-json".to_owned());
    Ok(part)
}

fn text_gap(id: &str, reason: &str) -> Value {
    object([
        ("id", Value::String(id.to_owned())),
        ("kind", Value::String("text".to_owned())),
        ("mediaType", Value::String("text/plain".to_owned())),
        ("selection", Value::String("body".to_owned())),
        (
            "gap",
            object([("reason", Value::String(reason.to_owned()))]),
        ),
    ])
}

fn textual_mime(mime: &str) -> bool {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/") || mime == "application/json" || mime.ends_with("+json")
}

fn immutable_inline(part: &Part, projection: &Value) -> bool {
    projection.get("gap").is_some() || data(part).is_some_and(|(_, mime)| textual_mime(mime))
}

fn output_part(output: &ToolOutput, id: &str) -> Result<Value, String> {
    match output {
        ToolOutput::Text(text) => Ok(text_part(id, text)),
        ToolOutput::Structured(value) => json_part(id, value),
        // Nested content needs a separate part-to-output mapping. Keep it native
        // and visible as an immutable gap rather than leaking an envelope.
        ToolOutput::Parts(_) | ToolOutput::Files(_) => Ok(text_gap(
            id,
            "native multipart tool result has no inline projection",
        )),
    }
}

fn restore_output(output: &ToolOutput, part: &Value) -> Result<ToolOutput, String> {
    match output {
        ToolOutput::Text(_) => Ok(ToolOutput::Text(inline_text(part)?.to_owned())),
        ToolOutput::Structured(_) => Ok(ToolOutput::Structured(
            serde_json::from_str(inline_text(part)?).map_err(|e| e.to_string())?,
        )),
        ToolOutput::Parts(_) | ToolOutput::Files(_) => {
            Err("nested multipart tool result is not supported by inline projection".into())
        }
    }
}

fn data(part: &Part) -> Option<(&DataRef, &str)> {
    match part {
        Part::Media(media) => Some((&media.data, &media.mime_type)),
        Part::File(file) => Some((
            &file.data,
            file.mime_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )),
        _ => None,
    }
}

fn project_part(part: &Part, id: &str) -> Result<Value, String> {
    if let Part::Text(text) = part {
        return Ok(text_part(id, &text.text));
    }
    if let Some((data, mime)) = data(part) {
        // Canonical attachment schemas exclude textual MIME types. InlineText
        // is actual text only for a textual MIME; never decode binary base64.
        if textual_mime(mime) {
            return Ok(match data {
                DataRef::InlineText(text) => text_part(id, text),
                _ => text_gap(id, "native textual attachment has no inline text owner"),
            });
        }
        // InlineText is not necessarily UTF-8 file content (it may be base64).
        // URI and Handle require an owning resolver; do not invent their bytes.
        return Ok(match data {
            DataRef::InlineBytes(bytes) => object([
                ("id", Value::String(id.to_owned())),
                ("kind", Value::String("attachment".to_owned())),
                ("mediaType", Value::String(mime.to_owned())),
                ("selection", Value::String("metadata".to_owned())),
                ("size", Value::Number(serde_json::Number::from(bytes.len()))),
            ]),
            _ => object([
                ("id", Value::String(id.to_owned())),
                ("kind", Value::String("attachment".to_owned())),
                ("mediaType", Value::String(mime.to_owned())),
                ("selection", Value::String("body".to_owned())),
                (
                    "gap",
                    object([(
                        "reason",
                        Value::String(
                            "native attachment reference has no resolved byte owner".to_owned(),
                        ),
                    )]),
                ),
            ]),
        });
    }
    // Expose only the domain payload, never the native Part envelope or private
    // metadata. The original variant defines exactly which field is editable.
    match part {
        Part::Structured(part) => json_part(id, &part.value),
        Part::ToolCall(part) => json_part(id, &part.input),
        Part::ToolResult(part) => output_part(&part.output, id),
        Part::Reasoning(part) => Ok(part
            .summary
            .as_deref()
            .map(|summary| text_part(id, summary))
            .unwrap_or_else(|| text_gap(id, "native reasoning has no exposable summary"))),
        Part::Custom(part) => match part.value.as_ref() {
            Some(value) => json_part(id, value),
            None => Ok(text_gap(id, "native custom content has no exposable value")),
        },
        Part::Text(_) | Part::Media(_) | Part::File(_) => {
            Err("invalid native part projection".into())
        }
    }
}

pub(super) fn messages(value: &Value, default_role: &str) -> Result<Value, String> {
    let (_, items) = snapshot(value, default_role)?;
    let ids = message_ids(&items)?;
    let messages = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let parts = item
                .parts
                .iter()
                .enumerate()
                .map(|(part_index, part)| project_part(part, &part_id(&ids[index], part_index)))
                .collect::<Result<Vec<_>, _>>()?;
            let mut message = object([
                ("id", Value::String(ids[index].clone())),
                ("role", Value::String(role(item.kind).to_owned())),
                ("parts", Value::Array(parts)),
            ]);
            if item.id.is_none() {
                message["synthesized"] = Value::Bool(true);
            }
            Ok(message)
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok(Value::Array(messages))
}

/// Actual native binary owners, with pointers relative to the messages array.
/// Bind these through the SDK's ContentSourceBinding API at the corresponding
/// event pointer. The SDK creates and owns immutable body references.
pub(super) fn attachments(
    value: &Value,
    default_role: &str,
) -> Result<Vec<(String, Attachment)>, String> {
    let (_, items) = snapshot(value, default_role)?;
    let mut sources = Vec::new();
    for (message_index, item) in items.iter().enumerate() {
        for (part_index, part) in item.parts.iter().enumerate() {
            if let Some((DataRef::InlineBytes(bytes), mime)) = data(part) {
                if textual_mime(mime) {
                    continue;
                }
                sources.push((
                    format!("/{message_index}/parts/{part_index}"),
                    Attachment::bytes(bytes.clone()),
                ));
            }
        }
    }
    Ok(sources)
}

fn restore_part(original: &Part, projected: &Value, id: &str) -> Result<Part, String> {
    let expected = project_part(original, id)?;
    if immutable_inline(original, &expected) {
        if projected != &expected {
            return Err("native opaque content and attachment sources are immutable".into());
        }
        return Ok(original.clone());
    }
    if let Part::Text(text) = original {
        let mut text = text.clone();
        text.text = inline_text(projected)?.to_owned();
        return Ok(Part::Text(text));
    }
    if data(original).is_some() {
        if projected["id"] != expected["id"]
            || projected["kind"] != expected["kind"]
            || projected["mediaType"] != expected["mediaType"]
        {
            return Err("native attachments are immutable".into());
        }
        // Body refs, hashes and selection are SDK-owned delivery metadata,
        // never a replacement byte source. Keep the native owner unchanged.
        if !matches!(
            projected["selection"].as_str(),
            Some("metadata" | "body" | "omit")
        ) {
            return Err("invalid native attachment selection".into());
        }
        return Ok(original.clone());
    }
    let text = inline_text(projected)?;
    let mut restored = original.clone();
    match &mut restored {
        Part::Structured(part) => {
            part.value = serde_json::from_str(text).map_err(|e| e.to_string())?
        }
        Part::ToolCall(part) => {
            part.input = serde_json::from_str(text).map_err(|e| e.to_string())?
        }
        Part::ToolResult(part) => part.output = restore_output(&part.output, projected)?,
        Part::Reasoning(part) => {
            if part.summary.is_none() {
                return Err("opaque reasoning has no inline summary".into());
            }
            part.summary = Some(text.to_owned());
        }
        Part::Custom(part) => {
            if part.value.is_none() {
                return Err("custom part has no inline value".into());
            }
            part.value = Some(serde_json::from_str(text).map_err(|e| e.to_string())?);
        }
        Part::Text(_) | Part::Media(_) | Part::File(_) => {
            return Err("invalid native part projection".into());
        }
    }
    Ok(restored)
}

pub(super) fn restore(original: &Value, messages: &Value) -> Result<Value, String> {
    let (shape, originals) = snapshot(original, "tool")?;
    let original_ids = message_ids(&originals)?;
    let messages = messages
        .as_array()
        .ok_or("expected canonical messages array")?;
    // A gap is not permission to discard opaque native state. Require every
    // immutable placeholder to survive, including when its whole item is dropped.
    for (index, item) in originals.iter().enumerate() {
        for (part_index, part) in item.parts.iter().enumerate() {
            let id = part_id(&original_ids[index], part_index);
            let projected = project_part(part, &id)?;
            if immutable_inline(part, &projected) {
                let retained = messages
                    .iter()
                    .find(|message| message["id"].as_str() == Some(original_ids[index].as_str()))
                    .and_then(|message| message["parts"].as_array())
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part["id"].as_str() == Some(id.as_str()))
                    });
                if !retained {
                    return Err("cannot remove immutable native opaque content".into());
                }
            }
        }
    }
    let mut ids = BTreeSet::new();
    let mut items = Vec::with_capacity(messages.len());
    for message in messages {
        let id = message["id"]
            .as_str()
            .ok_or("missing canonical message id")?;
        if !ids.insert(id) {
            return Err("duplicate canonical message id".into());
        }
        let kind = native_role(message["role"].as_str().ok_or("missing canonical role")?)?;
        let parts = message["parts"]
            .as_array()
            .ok_or("missing canonical parts")?;
        let old = originals
            .iter()
            .enumerate()
            .find(|(index, _)| original_ids[*index] == id)
            .map(|(_, item)| item);
        let mut item = old.cloned().unwrap_or_else(|| Item::new(kind, Vec::new()));
        // Preserve Context/Notification when their mapped canonical role is unchanged.
        item.kind = old
            .filter(|old| role(old.kind) == role(kind))
            .map_or(kind, |old| old.kind);
        let mut part_ids = BTreeSet::new();
        item.parts = parts
            .iter()
            .map(|part| {
                let id = part["id"].as_str().ok_or("missing canonical part id")?;
                if !part_ids.insert(id) {
                    return Err("duplicate canonical part id".into());
                }
                if let Some((_, original)) = old.and_then(|item| {
                    item.parts.iter().enumerate().find(|(index, _)| {
                        part_id(message["id"].as_str().unwrap_or_default(), *index) == id
                    })
                }) {
                    restore_part(original, part, id)
                } else {
                    // New content has no native provenance: allow only plain text.
                    Ok(Part::text(inline_text(part)?.to_owned()))
                }
            })
            .collect::<Result<Vec<_>, String>>()?;
        items.push(item);
    }
    let value = match shape {
        Shape::Array => serde_json::to_value(items),
        Shape::Item => {
            if items.len() != 1 {
                return Err("singleton native item requires one canonical message".into());
            }
            serde_json::to_value(&items[0])
        }
        Shape::Output(output) => {
            if items.len() != 1 {
                return Err("native tool output requires one canonical message".into());
            }
            let mut parts = items.remove(0).parts;
            let output = match output {
                ToolOutput::Text(_) => ToolOutput::Text(restore_text(&Value::Array(
                    parts
                        .iter()
                        .enumerate()
                        .map(|(index, part)| project_part(part, &part_id("tool-output", index)))
                        .collect::<Result<Vec<_>, _>>()?,
                ))?),
                ToolOutput::Structured(_) => {
                    if parts.len() != 1 {
                        return Err("structured output requires one structured part".into());
                    }
                    match parts.remove(0) {
                        Part::Structured(part) => ToolOutput::Structured(part.value),
                        _ => return Err("structured output requires a structured part".into()),
                    }
                }
                ToolOutput::Parts(_) => ToolOutput::Parts(parts),
                ToolOutput::Files(_) => ToolOutput::Files(
                    parts
                        .into_iter()
                        .map(|part| match part {
                            Part::File(file) => Ok(file),
                            _ => Err("file output requires native file parts".to_owned()),
                        })
                        .collect::<Result<_, _>>()?,
                ),
            };
            serde_json::to_value(output)
        }
    };
    value.map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_roles_and_provider_ids_survive_canonical_round_trip() {
        let original = serde_json::to_value(vec![
            Item::text(ItemKind::Context, "context").with_id("kit-message-1"),
            Item::text(ItemKind::Notification, "notification"),
        ])
        .unwrap();
        let projected = messages(&original, "user").unwrap();
        let _: agenthooksprotocol::generated::CanonicalMessages =
            serde_json::from_value(projected.clone()).unwrap();
        assert_eq!(projected[0]["id"], "kit-message-1");
        assert!(projected[0].get("synthesized").is_none());
        assert_ne!(projected[0]["id"], projected[1]["id"]);
        assert_ne!(
            projected[0]["parts"][0]["id"],
            projected[1]["parts"][0]["id"]
        );
        assert_eq!(projected[0]["role"], "system");
        assert_eq!(projected[1]["role"], "user");
        assert_eq!(restore(&original, &projected).unwrap(), original);
    }

    #[test]
    fn edits_touch_only_exposed_domain_payloads() {
        use agentkit_core::{CustomPart, ReasoningPart, ToolCallPart, ToolResultPart};
        let mut call = ToolCallPart::new("call-id", "native-tool", json!({"argument": 1}));
        call.metadata.insert("private".into(), json!("hidden"));
        let reasoning = ReasoningPart::summary("original summary")
            .with_data(DataRef::handle("private-reasoning"));
        let custom = CustomPart::new("private-kind")
            .with_value(json!({"custom": 1}))
            .with_data(DataRef::handle("private-custom"));
        let result = ToolResultPart::error("call-id", ToolOutput::text("original output"));
        let original_item = Item::new(
            ItemKind::Assistant,
            vec![
                Part::ToolCall(call),
                Part::Reasoning(reasoning),
                Part::Custom(custom),
                Part::ToolResult(result),
            ],
        );
        let original = serde_json::to_value(&original_item).unwrap();
        let mut projected = messages(&original, "assistant").unwrap();
        assert_eq!(projected[0]["parts"][0]["text"], "{\"argument\":1}");
        assert_eq!(projected[0]["parts"][1]["text"], "original summary");
        assert_eq!(projected[0]["parts"][2]["text"], "{\"custom\":1}");
        assert_eq!(projected[0]["parts"][3]["text"], "original output");
        assert!(!projected.to_string().contains("private"));
        projected[0]["parts"][0]["text"] = json!("{\"argument\":2}");
        projected[0]["parts"][1]["text"] = json!("updated summary");
        projected[0]["parts"][2]["text"] = json!("{\"custom\":2}");
        projected[0]["parts"][3]["text"] = json!("updated output");
        let mut expected = original_item;
        if let Part::ToolCall(part) = &mut expected.parts[0] {
            part.input = json!({"argument": 2});
        }
        if let Part::Reasoning(part) = &mut expected.parts[1] {
            part.summary = Some("updated summary".into());
        }
        if let Part::Custom(part) = &mut expected.parts[2] {
            part.value = Some(json!({"custom": 2}));
        }
        if let Part::ToolResult(part) = &mut expected.parts[3] {
            part.output = ToolOutput::text("updated output");
        }
        assert_eq!(
            restore(&original, &projected).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        projected[0]["parts"][0]["text"] = json!("not JSON");
        assert!(restore(&original, &projected).is_err());
    }

    #[test]
    fn opaque_content_uses_an_immutable_gap_without_native_envelopes() {
        let original = serde_json::to_value(Item::new(
            ItemKind::Assistant,
            vec![Part::Custom(
                agentkit_core::CustomPart::new("opaque").with_data(DataRef::handle("hidden")),
            )],
        ))
        .unwrap();
        let mut projected = messages(&original, "assistant").unwrap();
        let _: agenthooksprotocol::generated::CanonicalMessages =
            serde_json::from_value(projected.clone()).unwrap();
        assert!(projected[0]["parts"][0].get("gap").is_some());
        assert_eq!(restore(&original, &projected).unwrap(), original);
        assert!(restore(&original, &json!([])).is_err());
        let mut removed = projected.clone();
        removed[0]["parts"] = json!([]);
        assert!(restore(&original, &removed).is_err());
        projected[0]["parts"][0] =
            text_part(projected[0]["parts"][0]["id"].as_str().unwrap(), "invented");
        assert!(restore(&original, &projected).is_err());
    }

    #[test]
    fn opaque_reasoning_and_nested_tool_results_do_not_fail_projection() {
        use agentkit_core::{ReasoningPart, ToolResultPart};
        let mut reasoning = ReasoningPart::summary("unused");
        reasoning.summary = None;
        reasoning.data = Some(DataRef::handle("opaque-reasoning"));
        let item = Item::new(
            ItemKind::Assistant,
            vec![
                Part::Reasoning(reasoning),
                Part::ToolResult(ToolResultPart::success(
                    "call",
                    ToolOutput::parts(vec![
                        Part::text("nested"),
                        Part::file(DataRef::handle("nested-file")),
                    ]),
                )),
            ],
        );
        let original = serde_json::to_value(vec![item]).unwrap();
        let projected = messages(&original, "assistant").unwrap();
        let _: agenthooksprotocol::generated::CanonicalMessages =
            serde_json::from_value(projected.clone()).unwrap();
        for part in projected[0]["parts"].as_array().unwrap() {
            assert_eq!(part["kind"], "text");
            assert_eq!(part["selection"], "body");
            assert!(part.get("gap").is_some());
        }
        assert_eq!(restore(&original, &projected).unwrap(), original);
        assert!(restore(&original, &json!([])).is_err());
        let mut modified = projected.clone();
        modified[0]["parts"][1]["gap"]["reason"] = json!("replacement");
        assert!(restore(&original, &modified).is_err());
    }

    #[test]
    fn textual_file_sources_are_inline_or_gaps_never_binary_attachments() {
        use agentkit_core::FilePart;
        let mut inline = FilePart::new(DataRef::inline_text("{\"actual\":true}"));
        inline.mime_type = Some("application/json".into());
        let mut unresolved = FilePart::new(DataRef::uri("file:///must-not-read.txt"));
        unresolved.mime_type = Some("text/plain".into());
        let mut bytes = FilePart::new(DataRef::inline_bytes(b"text".to_vec()));
        bytes.mime_type = Some("text/plain".into());
        let original =
            serde_json::to_value(ToolOutput::files(vec![inline, unresolved, bytes])).unwrap();
        let projected = messages(&original, "tool").unwrap();
        let _: agenthooksprotocol::generated::CanonicalMessages =
            serde_json::from_value(projected.clone()).unwrap();
        assert_eq!(projected[0]["parts"][0]["text"], "{\"actual\":true}");
        assert!(projected[0]["parts"][1].get("gap").is_some());
        assert!(projected[0]["parts"][2].get("gap").is_some());
        assert!(attachments(&original, "tool").unwrap().is_empty());
        assert_eq!(restore(&original, &projected).unwrap(), original);
        let mut changed = projected.clone();
        changed[0]["parts"][0]["text"] = json!("replacement");
        assert!(restore(&original, &changed).is_err());
    }

    #[test]
    fn native_text_edit_preserves_item_and_part_metadata() {
        let mut item = Item::text(ItemKind::Assistant, "before").with_id("provider-id");
        item.metadata.insert("accounting".into(), json!(17));
        if let Part::Text(text) = &mut item.parts[0] {
            text.metadata.insert("origin".into(), json!("provider"));
        }
        let original = serde_json::to_value(&item).unwrap();
        let mut projected = messages(&original, "assistant").unwrap();
        projected[0]["parts"][0]["text"] = json!("after");
        let restored: Item =
            serde_json::from_value(restore(&original, &projected).unwrap()).unwrap();
        if let Part::Text(text) = &mut item.parts[0] {
            text.text = "after".into();
        }
        assert_eq!(restored, item);
    }

    #[test]
    fn structured_output_is_inline_editable_native_json() {
        let original = serde_json::to_value(ToolOutput::structured(json!({"answer": 1}))).unwrap();
        let mut projected = messages(&original, "tool").unwrap();
        assert_eq!(projected[0]["parts"][0]["kind"], "text");
        projected[0]["parts"][0]["text"] =
            json!(serde_json::to_string(&json!({"answer": 2})).unwrap());
        assert_eq!(
            restore(&original, &projected).unwrap(),
            serde_json::to_value(ToolOutput::structured(json!({"answer": 2}))).unwrap()
        );
    }

    #[test]
    fn prompt_messages_can_be_reordered_dropped_and_added() {
        let first = Item::text(ItemKind::User, "first").with_id("first-id");
        let second = Item::text(ItemKind::Assistant, "second").with_id("second-id");
        let original = serde_json::to_value(vec![first, second.clone()]).unwrap();
        let projected = messages(&original, "user").unwrap();
        let modified =
            json!([projected[1], {"id": "added", "role": "user", "parts": text_parts("new")}]);
        let restored: Vec<Item> =
            serde_json::from_value(restore(&original, &modified).unwrap()).unwrap();
        assert_eq!(restored, vec![second, Item::text(ItemKind::User, "new")]);
    }

    #[tokio::test]
    async fn sdk_bound_binary_body_restores_the_original_native_owner() {
        use agenthooksprotocol::{
            client::Decision,
            hooks::{Capabilities, EventGrant, Hooks, HooksOptions},
        };
        use std::collections::BTreeMap;

        let original = serde_json::to_value(vec![Item::new(
            ItemKind::User,
            vec![Part::file(DataRef::inline_bytes(vec![1, 2, 3]))],
        )])
        .unwrap();
        let projected = messages(&original, "user").unwrap();
        let mut sources = attachments(&original, "user")
            .unwrap()
            .into_iter()
            .map(|(pointer, source)| {
                let mut path = vec!["items".to_owned()];
                path.extend(
                    pointer
                        .trim_start_matches('/')
                        .split('/')
                        .map(str::to_owned),
                );
                super::super::BinarySource { path, source }
            })
            .collect();
        let options = HooksOptions::new(
            "urn:kit:test:projection",
            BTreeMap::from([
                (
                    "turn.start".to_owned(),
                    EventGrant::intercept(Capabilities::none()),
                ),
                ("tool.before".to_owned(), EventGrant::observe()),
            ]),
        );
        // Registration requires at least one hook. This nonmatching observer
        // validates registration without starting transport for turn.start.
        let hooks = Hooks::new(json!({"protocolVersion": "draft", "hooks": [{
            "id": "org.example.projection",
            "transport": {"type": "stdio", "command": "never-started", "lifecycle": "persistent"},
            "subscriptions": [{"events": ["tool.before"], "mode": "observe", "content": {"default": "metadata"}}]
        }]}), options).unwrap();
        let facts: agenthooksprotocol::inputs::TurnStartInput = serde_json::from_value(json!({"session": {"id": "session"}, "turn": {"id": "turn"}, "trigger": "external", "items": projected})).unwrap();
        let items =
            super::super::owned_messages(facts.items.clone(), &["items".into()], &mut sources)
                .unwrap();
        assert!(sources.is_empty());
        let settled = hooks
            .turn_start(facts.with_sources().with_items(items))
            .initial_state(Decision::Allow)
            .await
            .unwrap();
        let effective = &settled.effective_event["items"];
        assert_eq!(effective[0]["parts"][0]["selection"], "body");
        assert!(effective[0]["parts"][0]["body"]["ref"].as_str().is_some());
        assert_eq!(restore(&original, effective).unwrap(), original);
        // Removing actual binary parts is a list edit, not a byte mutation.
        // Unlike an unresolved opaque gap, it does not pin the native message.
        assert_eq!(restore(&original, &json!([])).unwrap(), json!([]));
        hooks.shutdown().await.unwrap();
    }

    #[test]
    fn binary_files_use_real_sdk_owners_and_valid_canonical_metadata() {
        let original = serde_json::to_value(Item::new(
            ItemKind::User,
            vec![Part::file(DataRef::inline_bytes(vec![1, 2, 3]))],
        ))
        .unwrap();
        let projected = messages(&original, "user").unwrap();
        let _: agenthooksprotocol::generated::CanonicalMessages =
            serde_json::from_value(projected.clone()).unwrap();
        assert_eq!(projected[0]["parts"][0]["selection"], "metadata");
        let sources = attachments(&original, "user").unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "/0/parts/0");
        assert_eq!(restore(&original, &projected).unwrap(), original);
    }

    #[test]
    fn unresolved_files_are_gaps_not_invented_bytes() {
        let original = serde_json::to_value(Item::new(
            ItemKind::User,
            vec![Part::file(DataRef::handle("artifact"))],
        ))
        .unwrap();
        let projected = messages(&original, "user").unwrap();
        assert_eq!(projected[0]["parts"][0]["selection"], "body");
        assert!(projected[0]["parts"][0].get("gap").is_some());
        let _: agenthooksprotocol::generated::CanonicalMessages =
            serde_json::from_value(projected.clone()).unwrap();
        assert!(attachments(&original, "user").unwrap().is_empty());
        assert_eq!(restore(&original, &projected).unwrap(), original);
    }
}
