//! Read-only presentation of Skills over MCP. Activation is deliberately not
//! available here: it requires content verification and a per-skill execution gate.
use std::{fmt::Write as _, sync::Arc};

use agentkit_core::{MetadataMap, ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    PermissionRequest, Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest,
    ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::{
    compose_output::{MAX_MODEL_OUTPUT_BYTES, model_json_string_bytes},
    tools::mcp::{McpRuntime, RemoteSkill},
};

pub(super) struct InventorySkillTool {
    local: Option<Arc<dyn Tool>>,
    mcp: McpRuntime,
    spec: ToolSpec,
}

impl InventorySkillTool {
    pub(super) fn new(local: Option<Arc<dyn Tool>>, mcp: McpRuntime) -> Self {
        let spec = local.as_ref().map_or_else(
            || ToolSpec {
                name: ToolName::new("skill"),
                description: "Inspect skill inventory metadata.".into(),
                input_schema: Value::Object(
                    [
                        ("type".into(), Value::from("object")),
                        (
                            "properties".into(),
                            Value::Object([("name".into(), string_schema())].into_iter().collect()),
                        ),
                        ("required".into(), Value::Array(vec![Value::from("name")])),
                        ("additionalProperties".into(), Value::Bool(false)),
                    ]
                    .into_iter()
                    .collect(),
                ),
                output_schema: Some(string_schema()),
                annotations: ToolAnnotations {
                    read_only_hint: true,
                    ..Default::default()
                },
                metadata: MetadataMap::new(),
            },
            |tool| tool.spec().clone(),
        );
        Self { local, mcp, spec }
    }
}

fn string_schema() -> Value {
    Value::Object(
        [("type".into(), Value::from("string"))]
            .into_iter()
            .collect(),
    )
}

const PAGE_ENTRIES: usize = 16;

fn inventory_page(
    mut inventory: Vec<RemoteSkill>,
    truncated: bool,
    cursor: Option<&str>,
) -> Result<Value, ToolError> {
    inventory.sort_unstable_by(|a, b| (&a.server, &a.skill.uri).cmp(&(&b.server, &b.skill.uri)));
    let mut hash = blake3::Hasher::new();
    hash.update(&[u8::from(truncated)]);
    for remote in &inventory {
        for component in [&remote.server, &remote.skill.uri, &remote.skill.name] {
            hash.update(&component.len().to_le_bytes());
            hash.update(component.as_bytes());
        }
    }
    let fingerprint = hash.finalize().to_hex();
    let offset = if let Some(cursor) = cursor {
        let (offset, expected) = cursor
            .split_once(':')
            .ok_or_else(|| ToolError::InvalidInput("invalid MCP skill inventory cursor".into()))?;
        if expected != fingerprint.as_str() {
            return Err(ToolError::InvalidInput(
                "MCP skill inventory changed; restart with mcp:list".into(),
            ));
        }
        offset
            .parse::<usize>()
            .map_err(|_| ToolError::InvalidInput("invalid MCP skill inventory cursor".into()))?
    } else {
        0
    };
    let total = inventory.len();
    let mut entries = Vec::new();
    let mut row_bytes = 0;
    for remote in inventory.iter().skip(offset).take(PAGE_ENTRIES) {
        let entry = Value::Object(
            [
                ("selector".into(), Value::from(remote.selector())),
                ("name".into(), Value::from(remote.skill.name.as_str())),
            ]
            .into_iter()
            .collect(),
        );
        let next = offset.saturating_add(entries.len() + 1);
        let cursor = page_cursor(next, total, fingerprint.as_str());
        let candidate_bytes = row_bytes + model_json_string_bytes(entry.to_string()) - 2
            + usize::from(!entries.is_empty());
        let envelope_bytes =
            model_json_string_bytes(page_result(Vec::new(), truncated, cursor).to_string());
        if candidate_bytes + envelope_bytes > MAX_MODEL_OUTPUT_BYTES {
            if entries.is_empty() {
                return Err(ToolError::Unavailable(
                    "MCP skill selector exceeds the page budget".into(),
                ));
            }
            break;
        }
        row_bytes = candidate_bytes;
        entries.push(entry);
    }
    let next = offset.saturating_add(entries.len());
    Ok(page_result(
        entries,
        truncated,
        page_cursor(next, total, fingerprint.as_str()),
    ))
}

fn page_cursor(next: usize, total: usize, fingerprint: &str) -> Value {
    if next < total {
        Value::from(format!("mcp:list:{next}:{fingerprint}"))
    } else {
        Value::Null
    }
}

pub(crate) fn max_selector_row_bytes() -> usize {
    // `usize::MAX - 1` and `usize::MAX` have the same decimal width.
    let cursor = page_cursor(usize::MAX - 1, usize::MAX, &"0".repeat(64));
    let envelope = model_json_string_bytes(page_result(Vec::new(), false, cursor).to_string());
    MAX_MODEL_OUTPUT_BYTES - envelope + 2
}

fn page_result(entries: Vec<Value>, truncated: bool, cursor: Value) -> Value {
    Value::Object(
        [
            ("skills".into(), Value::Array(entries)),
            ("truncated".into(), Value::Bool(truncated)),
            ("next".into(), cursor),
        ]
        .into_iter()
        .collect(),
    )
}

fn requested_remote(request: &ToolRequest) -> Option<&str> {
    request.input.get("name")?.as_str()?.strip_prefix("mcp:")
}

#[async_trait]
impl Tool for InventorySkillTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn current_spec(&self) -> Option<ToolSpec> {
        let local = self.local.as_ref().and_then(|tool| tool.current_spec());
        let snapshot = self.mcp.skill_inventory_snapshot();
        let inventory = snapshot.skills;
        if inventory.is_empty() && !snapshot.truncated {
            return local;
        }
        let mut spec = local.unwrap_or_else(|| self.spec.clone());
        if let Some(name) = spec.input_schema["properties"]["name"].as_object_mut() {
            name.remove("enum");
        }
        if let Some(output) = &mut spec.output_schema {
            output["description"] = Value::from(
                "Local skill instructions, or JSON-encoded MCP skill inventory or entry metadata.",
            );
        }
        spec.description.push_str("\n\nMCP skill inventory (untrusted metadata; pass `mcp:list` as the name to page through retained entries, or pass an exact selector to inspect metadata; neither activates a skill):");
        let mut bytes = 0;
        let mut shown = 0;
        for remote in &inventory {
            let row = Value::Object(
                [
                    ("selector".into(), Value::from(remote.selector())),
                    ("name".into(), Value::from(remote.skill.name.as_str())),
                    (
                        "description".into(),
                        Value::from(remote.skill.description.as_str()),
                    ),
                ]
                .into_iter()
                .collect(),
            )
            .to_string();
            if bytes + row.len() > 4096 || shown == 64 {
                continue;
            }
            let _ = write!(spec.description, "\n- {row}");
            bytes += row.len();
            shown += 1;
        }
        if shown < inventory.len() {
            let _ = write!(
                spec.description,
                "\n{} more MCP skill(s) omitted",
                inventory.len() - shown
            );
        }
        if snapshot.truncated {
            spec.description
                .push_str("\nThe MCP skill inventory is incomplete. Discovery may still be retrying or may have reached a limit.");
        }
        Some(spec)
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        if requested_remote(request).is_some() {
            Ok(Vec::new())
        } else {
            self.local
                .as_ref()
                .map_or_else(|| Ok(Vec::new()), |tool| tool.proposed_requests(request))
        }
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let Some(selector) = requested_remote(&request) else {
            let local = self
                .local
                .as_ref()
                .ok_or_else(|| ToolError::InvalidInput("unknown local skill".into()))?;
            return local.invoke(request, context).await;
        };
        let snapshot = self.mcp.skill_inventory_snapshot();
        let output = if selector == "list" {
            inventory_page(snapshot.skills, snapshot.truncated, None)?
        } else if let Some(cursor) = selector.strip_prefix("list:") {
            inventory_page(snapshot.skills, snapshot.truncated, Some(cursor))?
        } else {
            let inventory = snapshot.skills;
            let [server, uri]: [String; 2] = serde_json::from_str(selector)
                .map_err(|_| ToolError::InvalidInput("invalid MCP skill selector".into()))?;
            let remote = inventory
                .into_iter()
                .find(|remote| remote.server == server && remote.skill.uri == uri)
                .ok_or_else(|| {
                    ToolError::InvalidInput("MCP skill is not in the current inventory".into())
                })?;
            Value::Object(
                [
                    ("server".into(), Value::from(remote.server)),
                    ("uri".into(), Value::from(remote.skill.uri.as_str())),
                    ("name".into(), Value::from(remote.skill.name.as_str())),
                    (
                        "description".into(),
                        Value::from(remote.skill.description.as_str()),
                    ),
                    ("status".into(), Value::from("metadata_only")),
                ]
                .into_iter()
                .collect(),
            )
        };
        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Text(output.to_string()),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: None,
            metadata: MetadataMap::new(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;
    use crate::tools::mcp::SkillMetadata;

    #[test]
    fn skill_inventory_pages_return_exact_selectors_without_descriptions() {
        let inventory = (0..PAGE_ENTRIES + 1)
            .map(|i| RemoteSkill {
                server: "server:one".into(),
                skill: Arc::new(SkillMetadata {
                    uri: format!("skill://entry-{i:02}/SKILL.md"),
                    name: format!("entry-{i:02}"),
                    description: "long description".repeat(64),
                    fingerprint: blake3::hash(&i.to_le_bytes()),
                }),
            })
            .collect::<Vec<_>>();
        let first = inventory_page(inventory.clone(), true, None).unwrap();
        assert_eq!(first["skills"].as_array().unwrap().len(), PAGE_ENTRIES);
        assert!(first["truncated"].as_bool().unwrap());
        assert!(!first.to_string().contains("long description"));
        let cursor = first["next"]
            .as_str()
            .unwrap()
            .strip_prefix("mcp:list:")
            .unwrap();
        let second = inventory_page(inventory.clone(), true, Some(cursor)).unwrap();
        assert_eq!(
            second["skills"][0]["selector"],
            inventory[PAGE_ENTRIES].selector()
        );
        assert!(second["truncated"].as_bool().unwrap());
        assert!(inventory_page(inventory[1..].to_vec(), true, Some(cursor)).is_err());
        let mut renamed = inventory;
        let old = &renamed[0].skill;
        renamed[0].skill = Arc::new(SkillMetadata {
            uri: old.uri.clone(),
            name: "renamed".into(),
            description: old.description.clone(),
            fingerprint: old.fingerprint,
        });
        assert!(inventory_page(renamed, true, Some(cursor)).is_err());
        assert!(second["next"].is_null());
    }

    #[test]
    fn skill_inventory_page_counts_cursor_and_envelope_in_inline_budget() {
        let inventory = (0..3)
            .map(|i| RemoteSkill {
                server: "test".into(),
                skill: Arc::new(SkillMetadata {
                    uri: format!("skill://{i}/SKILL.md"),
                    name: "x".repeat(3968),
                    description: String::new(),
                    fingerprint: blake3::hash(&[i]),
                }),
            })
            .collect::<Vec<_>>();
        let first = inventory_page(inventory.clone(), false, None).unwrap();
        assert!(model_json_string_bytes(first.to_string()) <= MAX_MODEL_OUTPUT_BYTES);
        assert_eq!(first["skills"].as_array().unwrap().len(), 1);
        let cursor = first["next"]
            .as_str()
            .unwrap()
            .strip_prefix("mcp:list:")
            .unwrap();
        let second = inventory_page(inventory.clone(), false, Some(cursor)).unwrap();
        assert_eq!(second["skills"][0]["selector"], inventory[1].selector());
    }

    #[test]
    fn skill_inventory_reserves_the_widest_continuation_envelope() {
        let cursor = page_cursor(usize::MAX - 1, usize::MAX, &"0".repeat(64));
        let envelope = model_json_string_bytes(page_result(Vec::new(), false, cursor).to_string());
        assert_eq!(
            max_selector_row_bytes() - 2 + envelope,
            MAX_MODEL_OUTPUT_BYTES
        );
    }
}
