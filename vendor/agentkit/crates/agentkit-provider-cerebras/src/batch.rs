//! Batch API (feature = `batch`).
//!
//! Batch is async bulk `/v1/chat/completions` inference: upload a JSONL file
//! where each line is a complete chat-completion request, submit a batch job,
//! poll until terminal, then fetch the output / error file streams.
//!
//! The value-add of hosting this in the Cerebras crate is reuse of the same
//! [`crate::request::build_chat_body`] used by the turn loop, so preview-
//! feature conflicts and schema validation fail at submit time.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use agentkit_core::TurnCancellation;
use agentkit_http::{BodyStream, Http};
use agentkit_loop::TurnRequest;
use bytes::Bytes;
use futures_util::future::{Either, select};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::{CerebrasConfig, OutputFormat, ReasoningConfig};
use crate::error::CerebrasError;
use crate::files::{FilePurpose, FilesClient};
use crate::request::build_chat_body;

/// Batch job as returned by the Batch API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJob {
    /// Opaque batch identifier.
    pub id: String,
    /// Current status.
    pub status: BatchStatus,
    /// Target endpoint (always `"/v1/chat/completions"`).
    #[serde(default)]
    pub endpoint: String,
    /// Input JSONL file id.
    #[serde(default)]
    pub input_file_id: String,
    /// Output file id (populated once processing completes).
    #[serde(default)]
    pub output_file_id: Option<String>,
    /// Error-report file id.
    #[serde(default)]
    pub error_file_id: Option<String>,
    /// Aggregate counts by status.
    #[serde(default)]
    pub request_counts: BatchRequestCounts,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: u64,
    /// Expiry timestamp.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// When the job entered `finalizing`.
    #[serde(default)]
    pub finalizing_at: Option<u64>,
    /// When the job reached `completed`.
    #[serde(default)]
    pub completed_at: Option<u64>,
    /// When the job was cancelled.
    #[serde(default)]
    pub cancelled_at: Option<u64>,
    /// When the job failed.
    #[serde(default)]
    pub failed_at: Option<u64>,
    /// Opaque metadata attached at submit time.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Aggregate counts returned on `BatchJob`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchRequestCounts {
    /// Total requests in the batch.
    #[serde(default)]
    pub total: u64,
    /// Requests that completed successfully.
    #[serde(default)]
    pub completed: u64,
    /// Requests that failed.
    #[serde(default)]
    pub failed: u64,
}

/// Batch status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    /// Input file is being validated.
    Validating,
    /// Processing in progress.
    InProgress,
    /// Finalising output file.
    Finalizing,
    /// All requests processed, output file ready.
    Completed,
    /// Job failed (invalid input, etc.).
    Failed,
    /// Job expired without reaching completion.
    Expired,
    /// Cancellation in progress.
    Cancelling,
    /// Cancellation complete.
    Cancelled,
}

impl BatchStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Expired | Self::Cancelled
        )
    }
}

/// Per-line overrides applied on top of the adapter's `CerebrasConfig`.
/// A strict subset of the chat-completion knobs so each batch line can tune
/// itself without cloning the whole config.
#[derive(Debug, Clone, Default)]
pub struct ChatOverrides {
    /// Override model id.
    pub model: Option<String>,
    /// Override max_completion_tokens.
    pub max_completion_tokens: Option<u32>,
    /// Override temperature.
    pub temperature: Option<f32>,
    /// Override reasoning config.
    pub reasoning: Option<ReasoningConfig>,
    /// Override response_format.
    pub response_format: Option<OutputFormat>,
}

impl ChatOverrides {
    /// Applies the overrides onto a cloned `CerebrasConfig`. Returns the
    /// mutated config so the caller can feed it straight into `build_chat_body`.
    fn apply(&self, base: &CerebrasConfig) -> CerebrasConfig {
        let mut cfg = base.clone();
        if let Some(m) = &self.model {
            cfg.model = m.clone();
        }
        if let Some(v) = self.max_completion_tokens {
            cfg.max_completion_tokens = Some(v);
        }
        if let Some(v) = self.temperature {
            cfg.temperature = Some(v);
        }
        if let Some(r) = &self.reasoning {
            cfg.reasoning = Some(r.clone());
        }
        if let Some(f) = &self.response_format {
            cfg.output_format = Some(f.clone());
        }
        // Streaming never makes sense inside a JSONL line.
        cfg.streaming = false;
        cfg
    }
}

/// Batch API client bound to an adapter's `Http` + `CerebrasConfig`.
pub struct BatchClient<'a> {
    http: &'a Http,
    config: Arc<CerebrasConfig>,
}

/// Outcome of a terminal poll in [`BatchClient::wait`].
pub struct BatchOutcome {
    /// Final batch job.
    pub job: BatchJob,
    /// Output file content stream (unordered — match by `custom_id`).
    pub outputs: Option<BodyStream>,
    /// Error-report content stream.
    pub errors: Option<BodyStream>,
}

/// Input item for [`BatchClient::submit_chat_batch`].
pub type BatchItem = (String, TurnRequest, Option<ChatOverrides>);

/// Maximum total JSONL size (Cerebras docs).
const MAX_BATCH_BYTES: usize = 200 * 1024 * 1024;
/// Maximum line count per batch (Cerebras docs).
const MAX_BATCH_LINES: usize = 50_000;
/// Maximum bytes per line (Cerebras docs).
const MAX_LINE_BYTES: usize = 1024 * 1024;

impl<'a> BatchClient<'a> {
    /// Builds a new client — normally constructed via
    /// [`crate::CerebrasAdapter::batches`].
    pub fn new(http: &'a Http, config: Arc<CerebrasConfig>) -> Self {
        Self { http, config }
    }

    /// Assembles the JSONL input via [`crate::request::build_chat_body`],
    /// uploads it, and submits a batch job. Per-line overrides layer on top
    /// of the adapter's base config.
    pub async fn submit_chat_batch<I>(
        &self,
        items: I,
        metadata: BTreeMap<String, String>,
    ) -> Result<BatchJob, CerebrasError>
    where
        I: IntoIterator<Item = BatchItem>,
    {
        let mut jsonl: Vec<u8> = Vec::new();
        let mut line_count: usize = 0;
        for (custom_id, turn, overrides) in items {
            line_count += 1;
            if line_count > MAX_BATCH_LINES {
                return Err(CerebrasError::Other(format!(
                    "batch exceeds {MAX_BATCH_LINES} lines"
                )));
            }
            let cfg = match overrides {
                Some(ov) => ov.apply(&self.config),
                None => {
                    let mut c = (*self.config).clone();
                    c.streaming = false;
                    c
                }
            };
            let built = build_chat_body(&cfg, &turn)?;
            let mut obj = Map::new();
            obj.insert("custom_id".into(), Value::String(custom_id));
            obj.insert("method".into(), Value::String("POST".into()));
            obj.insert("url".into(), Value::String("/v1/chat/completions".into()));
            obj.insert("body".into(), built.body);
            let line = serde_json::to_vec(&Value::Object(obj))
                .map_err(|e| CerebrasError::Other(format!("batch line serialize: {e}")))?;
            if line.len() > MAX_LINE_BYTES {
                return Err(CerebrasError::Other(format!(
                    "batch line {line_count} exceeds {MAX_LINE_BYTES} bytes"
                )));
            }
            jsonl.extend_from_slice(&line);
            jsonl.push(b'\n');
            if jsonl.len() > MAX_BATCH_BYTES {
                return Err(CerebrasError::Other(format!(
                    "batch exceeds {MAX_BATCH_BYTES} bytes"
                )));
            }
        }

        let file = FilesClient::new(self.http, self.config.clone())
            .upload(
                &format!("batch-{}.jsonl", line_count),
                Bytes::from(jsonl),
                FilePurpose::Batch,
            )
            .await?;

        self.create(&file.id, metadata).await
    }

    /// Lower-level: create a batch job from an already-uploaded input file.
    pub async fn create(
        &self,
        input_file_id: &str,
        metadata: BTreeMap<String, String>,
    ) -> Result<BatchJob, CerebrasError> {
        let url = format!("{}/batches", self.config.base_url);
        let body = json!({
            "input_file_id": input_file_id,
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h",
            "metadata": metadata,
        });
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        Ok(response.json().await?)
    }

    /// Lists batch jobs visible to the configured API key.
    pub async fn list(&self) -> Result<Vec<BatchJob>, CerebrasError> {
        let url = format!("{}/batches", self.config.base_url);
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
        let raw: BatchList = response.json().await?;
        Ok(raw.data)
    }

    /// Retrieves a single batch job.
    pub async fn retrieve(&self, id: &str) -> Result<BatchJob, CerebrasError> {
        let url = format!("{}/batches/{id}", self.config.base_url);
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

    /// Cancels a batch job.
    pub async fn cancel(&self, id: &str) -> Result<BatchJob, CerebrasError> {
        let url = format!("{}/batches/{id}/cancel", self.config.base_url);
        let response = self
            .http
            .post(&url)
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

    /// Polls [`Self::retrieve`] until the job reaches a terminal status, then
    /// fetches the output + error file streams.
    ///
    /// `cancel` aborts the *wait* (not the batch itself — use
    /// [`Self::cancel`] for that).
    pub async fn wait(
        &self,
        id: &str,
        interval: Duration,
        cancel: Option<TurnCancellation>,
    ) -> Result<BatchOutcome, CerebrasError> {
        loop {
            let job = self.retrieve(id).await?;
            if job.status.is_terminal() {
                let files = crate::files::FilesClient::new(self.http, self.config.clone());
                let outputs = match &job.output_file_id {
                    Some(fid) => Some(files.content(fid).await?),
                    None => None,
                };
                let errors = match &job.error_file_id {
                    Some(fid) => Some(files.content(fid).await?),
                    None => None,
                };
                return Ok(BatchOutcome {
                    job,
                    outputs,
                    errors,
                });
            }

            let sleeper = futures_timer::Delay::new(interval);
            if let Some(cancel) = cancel.as_ref() {
                futures_util::pin_mut!(sleeper);
                let cancelled = cancel.cancelled();
                futures_util::pin_mut!(cancelled);
                match select(sleeper, cancelled).await {
                    Either::Left(((), _)) => {}
                    Either::Right(_) => {
                        return Err(CerebrasError::Other("wait cancelled".into()));
                    }
                }
            } else {
                sleeper.await;
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct BatchList {
    data: Vec<BatchJob>,
}
