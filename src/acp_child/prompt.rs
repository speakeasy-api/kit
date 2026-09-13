//! Typed child prompt input. Resource URIs are forwarded, never read by the parent.

use agentkit_acp::{ContentBlock, PromptCapabilities, TextContent};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ChildError;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ChildPrompt {
    Text(String),
    Blocks(#[serde(deserialize_with = "deserialize_blocks")] Vec<ContentBlock>),
}

fn deserialize_blocks<'de, D>(deserializer: D) -> Result<Vec<ContentBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let blocks = Vec::<ContentBlock>::deserialize(deserializer)?;
    if blocks.is_empty() {
        return Err(serde::de::Error::custom(
            "child prompt must contain at least one content block",
        ));
    }
    Ok(blocks)
}

impl From<String> for ChildPrompt {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for ChildPrompt {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

impl ChildPrompt {
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    pub(crate) fn into_blocks(
        self,
        capabilities: &PromptCapabilities,
    ) -> Result<Vec<ContentBlock>, ChildError> {
        let blocks = match self {
            Self::Text(text) => vec![ContentBlock::Text(TextContent::new(text))],
            Self::Blocks(blocks) => blocks,
        };
        if blocks.is_empty() {
            return Err(ChildError::Failed(
                "child prompt must contain at least one content block".into(),
            ));
        }
        for block in &blocks {
            let missing = match block {
                ContentBlock::Text(_) | ContentBlock::ResourceLink(_) => None,
                ContentBlock::Image(_) if capabilities.image => None,
                ContentBlock::Resource(_) if capabilities.embedded_context => None,
                ContentBlock::Image(_) => Some("promptCapabilities.image"),
                ContentBlock::Resource(_) => Some("promptCapabilities.embeddedContext"),
                _ => {
                    return Err(ChildError::Failed(
                        "unsupported child prompt content type".into(),
                    ));
                }
            };
            if let Some(capability) = missing {
                return Err(ChildError::Failed(format!(
                    "ACP child does not advertise {capability}; prompt was not sent"
                )));
            }
        }
        Ok(blocks)
    }
}

pub(crate) fn schema() -> Value {
    json!({
        "description": "Text or an ordered array of ACP content blocks. Resource links are forwarded without reading files. Images require the child's image capability; embedded resources require embeddedContext.",
        "oneOf": [
            {"type": "string"},
            {"type": "array", "minItems": 1, "items": {"oneOf": [
                {"type": "object", "properties": {"type": {"const": "text"}, "text": {"type": "string"}}, "required": ["type", "text"]},
                {"type": "object", "properties": {"type": {"const": "resource_link"}, "uri": {"type": "string"}, "name": {"type": "string"}, "mimeType": {"type": "string"}, "description": {"type": "string"}}, "required": ["type", "uri", "name"]},
                {"type": "object", "properties": {"type": {"const": "resource"}, "resource": {"oneOf": [
                    {"type": "object", "properties": {"uri": {"type": "string"}, "mimeType": {"type": "string"}, "text": {"type": "string"}}, "required": ["uri", "text"]},
                    {"type": "object", "properties": {"uri": {"type": "string"}, "mimeType": {"type": "string"}, "blob": {"type": "string"}}, "required": ["uri", "blob"]}
                ]}}, "required": ["type", "resource"]},
                {"type": "object", "properties": {"type": {"const": "image"}, "data": {"type": "string"}, "mimeType": {"type": "string"}, "uri": {"type": "string"}}, "required": ["type", "data", "mimeType"]}
            ]}}
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_capabilities_are_required_without_text_fallback() {
        for (block, capability) in [
            (
                json!({"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}),
                "image",
            ),
            (
                json!({"type": "resource", "resource": {"uri": "file:///context.txt", "text": "context"}}),
                "embeddedContext",
            ),
        ] {
            let prompt: ChildPrompt = serde_json::from_value(json!([block])).unwrap();
            let error = prompt
                .into_blocks(&PromptCapabilities::default())
                .unwrap_err();
            assert!(error.to_string().contains(capability));
        }
        assert!(serde_json::from_value::<ChildPrompt>(json!([])).is_err());
        let audio: ChildPrompt =
            serde_json::from_value(json!([{"type": "audio", "data": "", "mimeType": "audio/wav"}]))
                .unwrap();
        assert!(
            audio
                .into_blocks(&PromptCapabilities::default().audio(true))
                .is_err()
        );
    }

    #[test]
    fn links_and_text_need_no_optional_capability() {
        let blocks = json!([
            {"type": "text", "text": "Inspect this"},
            {"type": "resource_link", "uri": "file:///not-read-by-parent.txt", "name": "context", "mimeType": "text/plain"}
        ]);
        let prompt: ChildPrompt = serde_json::from_value(blocks.clone()).unwrap();
        assert_eq!(prompt.summary(), "Inspect this");
        let content = prompt.into_blocks(&PromptCapabilities::default()).unwrap();
        assert_eq!(serde_json::to_value(content).unwrap(), blocks);
        let text = ChildPrompt::from("legacy")
            .into_blocks(&PromptCapabilities::default())
            .unwrap();
        assert_eq!(
            serde_json::to_value(text).unwrap(),
            json!([{"type": "text", "text": "legacy"}])
        );
    }

    #[test]
    fn advertised_schema_matches_supported_content() {
        let validator = jsonschema::validator_for(&schema()).unwrap();
        for value in [
            json!("legacy"),
            json!([{"type": "text", "text": "hello"}]),
            json!([{"type": "resource", "resource": {"uri": "file:///blob", "blob": "YQ=="}}]),
        ] {
            assert!(validator.is_valid(&value));
            assert!(serde_json::from_value::<ChildPrompt>(value).is_ok());
        }
        for value in [
            json!([]),
            json!([{"type": "image", "data": "missing MIME type"}]),
            json!([{"type": "audio", "data": "", "mimeType": "audio/wav"}]),
        ] {
            assert!(!validator.is_valid(&value));
        }
    }
}
