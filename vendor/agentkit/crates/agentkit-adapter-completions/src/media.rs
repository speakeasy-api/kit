use agentkit_core::{DataRef, FilePart, MediaPart, Modality};
use base64::Engine;
use serde_json::{Value, json};

use crate::error::CompletionsError;

pub(crate) fn media_to_content(media: &MediaPart) -> Result<Value, CompletionsError> {
    match media.modality {
        Modality::Image => Ok(json!({
            "type": "image_url",
            "image_url": {
                "url": data_ref_to_url_like(&media.data, &media.mime_type)?,
            }
        })),
        Modality::Audio => Ok(json!({
            "type": "input_audio",
            "input_audio": {
                "data": data_ref_to_base64(&media.data)?,
                "format": audio_format_from_mime(&media.mime_type),
            }
        })),
        Modality::Video | Modality::Binary => {
            Err(CompletionsError::UnsupportedModality(media.modality))
        }
    }
}

pub(crate) fn file_to_content(file: &FilePart) -> Result<Value, CompletionsError> {
    match file.mime_type.as_deref() {
        Some(mime) if mime.starts_with("image/") => Ok(json!({
            "type": "image_url",
            "image_url": {
                "url": data_ref_to_url_like(&file.data, mime)?,
            }
        })),
        Some(mime) if mime.starts_with("audio/") => Ok(json!({
            "type": "input_audio",
            "input_audio": {
                "data": data_ref_to_base64(&file.data)?,
                "format": audio_format_from_mime(mime),
            }
        })),
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

pub(crate) fn data_ref_to_url_like(
    data: &DataRef,
    mime_type: &str,
) -> Result<String, CompletionsError> {
    match data {
        DataRef::Uri(uri) => Ok(uri.clone()),
        DataRef::InlineBytes(bytes) => Ok(format!(
            "data:{mime_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )),
        DataRef::InlineText(text) => {
            if text.starts_with("data:")
                || text.starts_with("http://")
                || text.starts_with("https://")
            {
                Ok(text.clone())
            } else {
                Err(CompletionsError::UnsupportedDataRef(
                    "image inputs must be a URL, data URL, or inline bytes".into(),
                ))
            }
        }
        DataRef::Handle(handle) => Err(CompletionsError::UnsupportedDataRef(format!(
            "artifact handle {} cannot be sent directly to the provider",
            handle.0
        ))),
    }
}

pub(crate) fn data_ref_to_base64(data: &DataRef) -> Result<String, CompletionsError> {
    match data {
        DataRef::InlineBytes(bytes) => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
        DataRef::InlineText(text) => {
            if let Some((_, encoded)) = text.split_once(";base64,") {
                Ok(encoded.to_string())
            } else {
                Ok(base64::engine::general_purpose::STANDARD.encode(text.as_bytes()))
            }
        }
        DataRef::Uri(uri) => Err(CompletionsError::UnsupportedDataRef(format!(
            "audio input URI {uri} must be loaded into bytes first"
        ))),
        DataRef::Handle(handle) => Err(CompletionsError::UnsupportedDataRef(format!(
            "artifact handle {} cannot be sent directly to the provider",
            handle.0
        ))),
    }
}

fn audio_format_from_mime(mime: &str) -> &'static str {
    match mime {
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        _ => "wav",
    }
}
