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

use crate::tools::mcp::{McpRuntime, RemoteSkill};

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
const PAGE_PREVIEW_BYTES: usize = 8 * 1024;

fn inventory_page(mut inventory: Vec<RemoteSkill>, offset: usize) -> Value {
    inventory.sort_unstable_by(|a, b| (&a.server, &a.skill.uri).cmp(&(&b.server, &b.skill.uri)));
    let total = inventory.len();
    let mut entries = Vec::new();
    let mut bytes = 0;
    for remote in inventory.iter().skip(offset).take(PAGE_ENTRIES) {
        let entry = Value::Object(
            [
                ("selector".into(), Value::from(remote.selector())),
                ("name".into(), Value::from(remote.skill.name.as_str())),
            ]
            .into_iter()
            .collect(),
        );
        bytes += entry.to_string().len();
        // Always return the first selector: it cannot be truncated without
        // changing the identity. Discovery limits the size of each entry.
        if bytes > PAGE_PREVIEW_BYTES && !entries.is_empty() {
            break;
        }
        entries.push(entry);
    }
    let next = offset.saturating_add(entries.len());
    Value::Object(
        [
            ("skills".into(), Value::Array(entries)),
            (
                "next".into(),
                if next < total {
                    Value::from(format!("mcp:list:{next}"))
                } else {
                    Value::Null
                },
            ),
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
        let inventory = self.mcp.skill_inventory();
        if inventory.is_empty() {
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
        spec.description.push_str("\n\nMCP skill inventory (untrusted metadata; pass `mcp:list` as the name to page through all entries, or pass an exact selector to inspect metadata; neither activates a skill):");
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
        let inventory = self.mcp.skill_inventory();
        let output = if selector == "list" {
            inventory_page(inventory, 0)
        } else if let Some(offset) = selector.strip_prefix("list:") {
            let offset = offset.parse::<usize>().map_err(|_| {
                ToolError::InvalidInput("invalid MCP skill inventory cursor".into())
            })?;
            inventory_page(inventory, offset)
        } else {
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
        let first = inventory_page(inventory.clone(), 0);
        assert_eq!(first["skills"].as_array().unwrap().len(), PAGE_ENTRIES);
        assert_eq!(first["next"], format!("mcp:list:{PAGE_ENTRIES}"));
        assert!(!first.to_string().contains("long description"));
        let second = inventory_page(inventory.clone(), PAGE_ENTRIES);
        assert_eq!(
            second["skills"][0]["selector"],
            inventory[PAGE_ENTRIES].selector()
        );
        assert!(second["next"].is_null());
    }
}
