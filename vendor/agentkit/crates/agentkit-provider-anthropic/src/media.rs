use agentkit_core::{DataRef, FilePart, MediaPart, Modality};
use base64::Engine;
use serde_json::{Value, json};

use crate::request::BuildError;

pub(crate) fn media_to_content(media: &MediaPart) -> Result<Value, BuildError> {
    match media.modality {
        Modality::Image => Ok(json!({
            "type": "image",
            "source": image_source(&media.data, &media.mime_type)?,
        })),
        Modality::Audio | Modality::Video | Modality::Binary => {
            Err(BuildError::UnsupportedModality(media.modality))
        }
    }
}

pub(crate) fn file_to_content(file: &FilePart) -> Result<Value, BuildError> {
    match file.mime_type.as_deref() {
        Some(mime) if mime.starts_with("image/") => Ok(json!({
            "type": "image",
            "source": image_source(&file.data, mime)?,
        })),
        Some("application/pdf") | Some("text/plain") => {
            let mut block = json!({
                "type": "document",
                "source": document_source(&file.data, file.mime_type.as_deref())?,
            });
            if let Some(name) = &file.name
                && let Some(map) = block.as_object_mut()
            {
                map.insert("title".into(), Value::String(name.clone()));
            }
            Ok(block)
        }
        _ => Ok(json!({
            "type": "text",
            "text": format!(
                "Attached file{}{}",
                file.name
                    .as_ref()
                    .map(|name| format!(": {name}"))
                    .unwrap_or_default(),
                file.mime_type
                    .as_ref()
                    .map(|mime| format!(" ({mime})"))
                    .unwrap_or_default(),
            ),
        })),
    }
}

fn image_source(data: &DataRef, mime_type: &str) -> Result<Value, BuildError> {
    match data {
        DataRef::Uri(uri) => Ok(json!({ "type": "url", "url": uri })),
        DataRef::InlineBytes(bytes) => Ok(json!({
            "type": "base64",
            "media_type": mime_type,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        })),
        DataRef::InlineText(text) => {
            if let Some((header, encoded)) = text.split_once(";base64,") {
                let media_type = header.strip_prefix("data:").unwrap_or(mime_type);
                Ok(json!({
                    "type": "base64",
                    "media_type": media_type,
                    "data": encoded,
                }))
            } else if text.starts_with("http://") || text.starts_with("https://") {
                Ok(json!({ "type": "url", "url": text }))
            } else {
                Err(BuildError::UnsupportedDataRef(
                    "image inputs must be a URL, data URL, or inline bytes".into(),
                ))
            }
        }
        DataRef::Handle(handle) => Err(BuildError::UnsupportedDataRef(format!(
            "artifact handle {} cannot be sent directly to Anthropic",
            handle.0
        ))),
    }
}

fn document_source(data: &DataRef, mime_type: Option<&str>) -> Result<Value, BuildError> {
    let mime = mime_type.unwrap_or("application/pdf");
    match data {
        DataRef::Uri(uri) => Ok(json!({ "type": "url", "url": uri })),
        DataRef::InlineBytes(bytes) => Ok(json!({
            "type": "base64",
            "media_type": mime,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        })),
        DataRef::InlineText(text) => {
            if mime == "text/plain" {
                Ok(json!({
                    "type": "text",
                    "media_type": "text/plain",
                    "data": text,
                }))
            } else if let Some((_, encoded)) = text.split_once(";base64,") {
                Ok(json!({
                    "type": "base64",
                    "media_type": mime,
                    "data": encoded,
                }))
            } else if text.starts_with("http://") || text.starts_with("https://") {
                Ok(json!({ "type": "url", "url": text }))
            } else {
                Err(BuildError::UnsupportedDataRef(
                    "document inputs must be a URL, data URL, or inline bytes".into(),
                ))
            }
        }
        DataRef::Handle(handle) => Err(BuildError::UnsupportedDataRef(format!(
            "artifact handle {} cannot be sent directly to Anthropic",
            handle.0
        ))),
    }
}
