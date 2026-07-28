//! Files API (feature = `batch`).
//!
//! Cerebras files exist solely to feed the Batch API (`purpose = "batch"`);
//! the crate does not expose any other `purpose` today.

use std::sync::Arc;

use agentkit_http::{BodyStream, Http};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::config::CerebrasConfig;
use crate::error::CerebrasError;

/// File purpose. Only `Batch` is surfaced because the batch pipeline is the
/// only in-scope consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilePurpose {
    /// `purpose = "batch"` — the sole supported value.
    Batch,
}

impl FilePurpose {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Batch => "batch",
        }
    }
}

/// A single file descriptor as returned by the Files API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileObject {
    /// Opaque file identifier used when submitting a batch.
    pub id: String,
    /// Size in bytes.
    #[serde(default)]
    pub bytes: u64,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: u64,
    /// Original filename.
    #[serde(default)]
    pub filename: String,
    /// Declared purpose.
    pub purpose: FilePurpose,
}

/// Files API client bound to an adapter's `Http` + `CerebrasConfig`.
pub struct FilesClient<'a> {
    http: &'a Http,
    config: Arc<CerebrasConfig>,
}

impl<'a> FilesClient<'a> {
    /// Builds a new client — typically constructed via
    /// [`crate::CerebrasAdapter::files`].
    pub fn new(http: &'a Http, config: Arc<CerebrasConfig>) -> Self {
        Self { http, config }
    }

    /// Uploads a JSONL batch file. Returns the created [`FileObject`].
    pub async fn upload(
        &self,
        filename: &str,
        bytes: impl Into<Bytes>,
        purpose: FilePurpose,
    ) -> Result<FileObject, CerebrasError> {
        let url = format!("{}/files", self.config.base_url);
        let boundary = format!(
            "----cerebras{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let body = build_multipart(&boundary, filename, bytes.into(), purpose);
        let content_type = format!("multipart/form-data; boundary={boundary}");
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .header("Content-Type", content_type)
            .body(body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        Ok(response.json().await?)
    }

    /// Lists every file uploaded under the configured API key.
    pub async fn list(&self) -> Result<Vec<FileObject>, CerebrasError> {
        let url = format!("{}/files", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        let raw: FileList = response.json().await?;
        Ok(raw.data)
    }

    /// Retrieves a single file descriptor.
    pub async fn retrieve(&self, id: &str) -> Result<FileObject, CerebrasError> {
        let url = format!("{}/files/{id}", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        Ok(response.json().await?)
    }

    /// Streams a file's content (batch output or error reports).
    pub async fn content(&self, id: &str) -> Result<BodyStream, CerebrasError> {
        let url = format!("{}/files/{id}/content", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        Ok(response.bytes_stream())
    }

    /// Deletes a file.
    pub async fn delete(&self, id: &str) -> Result<(), CerebrasError> {
        let url = format!("{}/files/{id}", self.config.base_url);
        let response = self
            .http
            .delete(&url)
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct FileList {
    data: Vec<FileObject>,
}

fn build_multipart(
    boundary: &str,
    filename: &str,
    file_bytes: Bytes,
    purpose: FilePurpose,
) -> Bytes {
    let mut body: Vec<u8> = Vec::with_capacity(file_bytes.len() + 512);
    // purpose field
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"purpose\"\r\n\r\n");
    body.extend_from_slice(purpose.as_str().as_bytes());
    body.extend_from_slice(b"\r\n");
    // file field
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(&file_bytes);
    body.extend_from_slice(b"\r\n");
    // closing boundary
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    Bytes::from(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_body_has_purpose_and_file_parts() {
        let body = build_multipart(
            "BND",
            "x.jsonl",
            Bytes::from_static(b"{}\n"),
            FilePurpose::Batch,
        );
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("name=\"purpose\""));
        assert!(text.contains("batch"));
        assert!(text.contains("name=\"file\""));
        assert!(text.contains("filename=\"x.jsonl\""));
        assert!(text.contains("{}\n"));
        assert!(text.trim_end().ends_with("--BND--"));
    }
}
