use std::collections::VecDeque;

use agentkit_core::{
    DataRef, Delta, FinishReason, Item, ItemKind, MediaPart, MetadataMap, Modality, Part,
    ReasoningPart, TextPart, TokenUsage, ToolCallPart, Usage,
};
use agentkit_loop::{ModelTurnEvent, ModelTurnResult};
use serde::Deserialize;
use serde_json::Value;

use crate::CompletionsProvider;
use crate::error::CompletionsError;

pub(crate) fn build_turn_from_response<P: CompletionsProvider>(
    provider: &P,
    body: &str,
) -> Result<(VecDeque<ModelTurnEvent>, Value), CompletionsError> {
    let raw: Value =
        serde_json::from_str(body).map_err(|e| CompletionsError::Protocol(format!("{e}")))?;

    let response: ChatCompletionResponse = serde_json::from_value(raw.clone())
        .map_err(|e| CompletionsError::Protocol(format!("{e}")))?;

    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| CompletionsError::Protocol("response contained no choices".into()))?;

    let mut events = VecDeque::new();

    let mut usage = map_usage(response.usage);
    let mut response_metadata = MetadataMap::new();

    // Let the provider enrich usage and metadata from the raw response
    provider.postprocess_response(&mut usage, &mut response_metadata, &raw);

    if let Some(ref usage) = usage {
        events.push_back(ModelTurnEvent::Usage(usage.clone()));
    }

    let message = choice.message;
    let mut parts = message_to_parts(&message)?;
    let finish_reason = map_finish_reason(choice.finish_reason.as_deref());
    let response_model = response.model;
    let response_id = response.id;

    for part in &parts {
        if let Part::ToolCall(call) = part {
            events.push_back(ModelTurnEvent::ToolCall(call.clone()));
        }
    }

    if !parts.is_empty() {
        let assistant_item = Item {
            id: response_id.clone().map(Into::into),
            kind: ItemKind::Assistant,
            parts: std::mem::take(&mut parts),
            metadata: response_metadata,
            usage: None,
            finish_reason: None,
            created_at: None,
        };

        for part in &assistant_item.parts {
            events.push_back(ModelTurnEvent::Delta(Delta::CommitPart {
                part: part.clone(),
            }));
        }

        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason,
            output_items: vec![assistant_item],
            usage,
            metadata: MetadataMap::new(),
            model: response_model,
            response_id,
        }));
    } else {
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason,
            output_items: Vec::new(),
            usage,
            metadata: MetadataMap::new(),
            model: response_model,
            response_id,
        }));
    }

    Ok((events, raw))
}

fn message_to_parts(message: &ResponseMessage) -> Result<Vec<Part>, CompletionsError> {
    let mut parts = Vec::new();

    if let Some(reasoning) = &message.reasoning {
        parts.push(Part::Reasoning(ReasoningPart {
            summary: Some(reasoning.clone()),
            data: None,
            redacted: false,
            metadata: MetadataMap::new(),
        }));
    }

    if let Some(reasoning_details) = &message.reasoning_details
        && !reasoning_details.is_null()
    {
        parts.push(Part::Reasoning(ReasoningPart {
            summary: None,
            data: None,
            redacted: false,
            metadata: MetadataMap::from([(
                "completions.reasoning_details".into(),
                reasoning_details.clone(),
            )]),
        }));
    }

    if let Some(content) = &message.content {
        parts.extend(content_to_parts(content)?);
    }

    for image in &message.images {
        parts.push(Part::Media(MediaPart {
            modality: Modality::Image,
            mime_type: "image/*".into(),
            data: DataRef::Uri(image.image_url.url.clone()),
            metadata: MetadataMap::new(),
        }));
    }

    for tool_call in &message.tool_calls {
        parts.push(Part::ToolCall(ToolCallPart {
            id: tool_call.id.clone().into(),
            name: tool_call.function.name.clone(),
            input: parse_tool_arguments(&tool_call.function.arguments)?,
            metadata: MetadataMap::new(),
        }));
    }

    Ok(parts)
}

fn content_to_parts(content: &ResponseContent) -> Result<Vec<Part>, CompletionsError> {
    match content {
        ResponseContent::Text(text) => Ok(vec![Part::Text(TextPart {
            text: text.clone(),
            metadata: MetadataMap::new(),
        })]),
        ResponseContent::Parts(parts) => {
            let mut normalized = Vec::new();
            for part in parts {
                match part.kind.as_str() {
                    "text" => {
                        if let Some(text) = &part.text {
                            normalized.push(Part::Text(TextPart {
                                text: text.clone(),
                                metadata: MetadataMap::new(),
                            }));
                        }
                    }
                    "image_url" => {
                        if let Some(image_url) = &part.image_url {
                            normalized.push(Part::Media(MediaPart {
                                modality: Modality::Image,
                                mime_type: "image/*".into(),
                                data: DataRef::Uri(image_url.url.clone()),
                                metadata: MetadataMap::new(),
                            }));
                        }
                    }
                    "input_audio" => {
                        if let Some(audio) = &part.input_audio {
                            normalized.push(Part::Media(MediaPart {
                                modality: Modality::Audio,
                                mime_type: format!("audio/{}", audio.format),
                                data: DataRef::InlineText(format!(
                                    "data:audio/{};base64,{}",
                                    audio.format, audio.data
                                )),
                                metadata: MetadataMap::new(),
                            }));
                        }
                    }
                    other => {
                        normalized.push(Part::Custom(agentkit_core::CustomPart {
                            kind: format!("completions.content.{other}"),
                            data: None,
                            value: Some(
                                serde_json::to_value(part).map_err(CompletionsError::Serialize)?,
                            ),
                            metadata: MetadataMap::new(),
                        }));
                    }
                }
            }
            Ok(normalized)
        }
    }
}

pub(crate) fn parse_tool_arguments(arguments: &str) -> Result<Value, CompletionsError> {
    serde_json::from_str(arguments).map_err(|error| {
        CompletionsError::Protocol(format!(
            "invalid tool arguments JSON {arguments:?}: {error}"
        ))
    })
}

pub(crate) fn map_usage(usage: Option<ResponseUsage>) -> Option<Usage> {
    usage.map(|usage| Usage {
        tokens: Some(TokenUsage {
            input_tokens: usage.prompt_tokens.unwrap_or_default(),
            output_tokens: usage.completion_tokens.unwrap_or_default(),
            reasoning_tokens: usage
                .completion_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens),
            cached_input_tokens: usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            cache_write_input_tokens: usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cache_write_tokens),
        }),
        cost: None,
        metadata: MetadataMap::new(),
    })
}

pub(crate) fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") => FinishReason::Completed,
        Some("tool_calls") => FinishReason::ToolCall,
        Some("length") => FinishReason::MaxTokens,
        Some("content_filter") => FinishReason::Blocked,
        Some("cancelled") => FinishReason::Cancelled,
        Some(other) => FinishReason::Other(other.into()),
        None => FinishReason::Completed,
    }
}

// --- Response deserialization structs ---

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ChatCompletionResponse {
    pub id: Option<String>,
    pub model: Option<String>,
    pub choices: Vec<ResponseChoice>,
    pub usage: Option<ResponseUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseChoice {
    pub message: ResponseMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ResponseMessage {
    pub content: Option<ResponseContent>,
    #[serde(default)]
    pub tool_calls: Vec<ResponseToolCall>,
    pub reasoning: Option<String>,
    pub reasoning_details: Option<Value>,
    pub refusal: Option<String>,
    /// De facto convention across OpenAI-compatible gateways for returning
    /// generated images from image-output models routed via chat completions —
    /// the OpenAI spec never specified where output images should live, so
    /// gateways converged on a top-level `images` array on the message,
    /// alongside `content`/`tool_calls`.
    ///
    /// Observed in production at:
    ///   - OpenRouter (Google Nano Banana family, others)
    ///   - Vercel AI Gateway (Nano Banana, Nano Banana Pro, GPT-5 image variants)
    ///   - llmgateway.io
    ///
    /// Each entry mirrors the OpenAI `image_url` part shape — `image_url.url`
    /// typically a `data:image/...;base64,...` URL.
    #[serde(default)]
    pub images: Vec<ResponseImage>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ResponseContent {
    Text(String),
    Parts(Vec<ResponseContentPart>),
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct ResponseContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
    pub image_url: Option<ResponseImageUrl>,
    pub input_audio: Option<ResponseInputAudio>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct ResponseImageUrl {
    pub url: String,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct ResponseImage {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub image_url: ResponseImageUrl,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct ResponseInputAudio {
    pub data: String,
    pub format: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseToolCall {
    pub id: String,
    pub function: ResponseFunction,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub(crate) struct ResponseUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub prompt_tokens_details: Option<ResponsePromptTokenDetails>,
    pub completion_tokens_details: Option<ResponseCompletionTokenDetails>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub(crate) struct ResponsePromptTokenDetails {
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub(crate) struct ResponseCompletionTokenDetails {
    pub reasoning_tokens: Option<u64>,
}
