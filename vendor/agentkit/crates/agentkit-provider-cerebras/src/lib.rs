//! Cerebras Inference API adapter for the agentkit agent loop.
//!
//! This crate implements the agentkit [`ModelAdapter`] directly against
//! Cerebras' `/v1/chat/completions` endpoint.
//!
//! Streaming is on by default. Toggle via [`CerebrasConfig::with_streaming`].
//!
//! # Quick start
//!
//! ```rust,ignore
//! use agentkit_loop::{Agent, SessionConfig};
//! use agentkit_provider_cerebras::{CerebrasAdapter, CerebrasConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = CerebrasConfig::from_env()?;
//!     let adapter = CerebrasAdapter::new(config)?;
//!     let agent = Agent::builder().model(adapter).build()?;
//!     let _driver = agent.start(SessionConfig::new("demo")).await?;
//!     Ok(())
//! }
//! ```

pub mod config;
pub mod error;
pub mod models;
pub mod rate_limit;
pub mod request;
pub mod response;
pub mod version;

#[cfg(feature = "batch")]
pub mod batch;
#[cfg(feature = "compression")]
pub mod compression;
#[cfg(feature = "batch")]
pub mod files;

mod sse;
mod stream;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agentkit_core::TurnCancellation;
use agentkit_http::{BodyStream, Http, HttpError, HttpRequestBuilder};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, SessionConfig, TurnRequest,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::future::{Either, select};

pub use crate::config::{
    CerebrasConfig, DEFAULT_BASE_URL, DEFAULT_VERSION_PATCH, OutputFormat, PartKindName,
    ReasoningConfig, ReasoningEffort, ReasoningFormat, ToolChoice,
};
pub use crate::error::{BuildError, CerebrasError, ResponseError};
pub use crate::models::{ModelObject, ModelsClient};
pub use crate::rate_limit::RateLimitSnapshot;

#[cfg(feature = "predicted-outputs")]
pub use crate::config::Prediction;
#[cfg(feature = "compression")]
pub use crate::config::{CompressionConfig, RequestEncoding};
#[cfg(feature = "service-tiers")]
pub use crate::config::{QueueThreshold, ServiceTier};

#[cfg(feature = "batch")]
pub use crate::batch::{
    BatchClient, BatchItem, BatchJob, BatchOutcome, BatchRequestCounts, BatchStatus, ChatOverrides,
};
#[cfg(feature = "batch")]
pub use crate::files::{FileObject, FilePurpose, FilesClient};

use crate::stream::{EventTranslator, SseDecoder};

/// Model adapter that connects the agentkit agent loop to Cerebras'
/// `/v1/chat/completions` endpoint.
#[derive(Clone)]
pub struct CerebrasAdapter {
    client: Http,
    config: Arc<CerebrasConfig>,
    last_rate_limit: Arc<Mutex<Option<RateLimitSnapshot>>>,
}

impl CerebrasAdapter {
    /// Creates a new adapter from the given configuration, building a default
    /// reqwest-backed HTTP client.
    pub fn new(config: CerebrasConfig) -> Result<Self, CerebrasError> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .build()
            .map(Http::new)
            .map_err(|error| CerebrasError::Http(HttpError::request(error)))?;
        Ok(Self {
            client,
            config: Arc::new(config),
            last_rate_limit: Arc::new(Mutex::new(None)),
        })
    }

    /// Creates a new adapter using a pre-configured [`Http`] client.
    pub fn with_client(config: CerebrasConfig, client: Http) -> Result<Self, CerebrasError> {
        config.validate()?;
        Ok(Self {
            client,
            config: Arc::new(config),
            last_rate_limit: Arc::new(Mutex::new(None)),
        })
    }

    /// Reads the latest rate-limit snapshot, if any response has been received.
    pub fn last_rate_limit(&self) -> Option<RateLimitSnapshot> {
        self.last_rate_limit.lock().ok()?.clone()
    }

    /// Returns a typed client over `/v1/models`.
    pub fn models(&self) -> ModelsClient<'_> {
        ModelsClient::new(&self.client, self.config.clone())
    }

    /// Returns a typed client over the Batch API.
    #[cfg(feature = "batch")]
    pub fn batches(&self) -> BatchClient<'_> {
        BatchClient::new(&self.client, self.config.clone())
    }

    /// Returns a typed client over the Files API.
    #[cfg(feature = "batch")]
    pub fn files(&self) -> FilesClient<'_> {
        FilesClient::new(&self.client, self.config.clone())
    }
}

/// An active session against the Cerebras chat-completions endpoint.
pub struct CerebrasSession {
    client: Http,
    config: Arc<CerebrasConfig>,
    rate_limit_slot: Arc<Mutex<Option<RateLimitSnapshot>>>,
    _session_config: SessionConfig,
}

/// A single Cerebras chat-completions turn in progress.
pub struct CerebrasTurn {
    inner: TurnInner,
}

enum TurnInner {
    Buffered { events: VecDeque<ModelTurnEvent> },
    Streaming(Box<StreamingState>),
}

struct StreamingState {
    body: BodyStream,
    decoder: SseDecoder,
    translator: EventTranslator,
    pending: VecDeque<ModelTurnEvent>,
    eof: bool,
}

#[async_trait]
impl ModelAdapter for CerebrasAdapter {
    type Session = CerebrasSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(CerebrasSession {
            client: self.client.clone(),
            config: self.config.clone(),
            rate_limit_slot: self.last_rate_limit.clone(),
            _session_config: config,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("cerebras")
    }
}

#[async_trait]
impl ModelSession for CerebrasSession {
    type Turn = CerebrasTurn;

    async fn begin_turn(
        &mut self,
        turn_request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<CerebrasTurn, LoopError> {
        let config = self.config.clone();
        let rate_limit_slot = self.rate_limit_slot.clone();

        let request_future = async move {
            let built = request::build_chat_body(&config, &turn_request)
                .map_err(|e| LoopError::Provider(e.to_string()))?;

            let url = format!("{}/chat/completions", config.base_url);
            let mut http = self.client.post(&url).bearer_auth(&config.api_key);

            #[cfg(feature = "compression")]
            let (body_bytes, content_type, content_encoding) = match &config.compression {
                Some(cfg) => {
                    let encoded = crate::compression::encode_body(&built.body, cfg)
                        .map_err(LoopError::Provider)?;
                    (
                        bytes::Bytes::from(encoded.body),
                        encoded.content_type,
                        encoded.content_encoding,
                    )
                }
                None => (
                    bytes::Bytes::from(
                        serde_json::to_vec(&built.body)
                            .map_err(|e| LoopError::Provider(format!("json serialize: {e}")))?,
                    ),
                    "application/json",
                    None,
                ),
            };
            #[cfg(not(feature = "compression"))]
            let (body_bytes, content_type, content_encoding) = (
                bytes::Bytes::from(
                    serde_json::to_vec(&built.body)
                        .map_err(|e| LoopError::Provider(format!("json serialize: {e}")))?,
                ),
                "application/json",
                None::<&'static str>,
            );

            http = http.header("Content-Type", content_type);
            if let Some(enc) = content_encoding {
                http = http.header("Content-Encoding", enc);
            }
            if let Some(patch) = config.version_patch {
                http = http.header(
                    crate::version::VERSION_PATCH_HEADER,
                    crate::version::format_version_patch(patch),
                );
            }
            for (k, v) in &built.extra_headers {
                http = http.header(*k, v.clone());
            }
            http = http.header(
                "User-Agent",
                concat!("agentkit-provider-cerebras/", env!("CARGO_PKG_VERSION")),
            );
            if config.streaming {
                http = http.header("Accept", "text/event-stream");
            }
            for (k, v) in &config.extra_headers {
                http = http.header(k.as_str(), v.as_str());
            }
            http = attach_body(http, body_bytes);

            let response = http.send().await.map_err(|error| {
                LoopError::Provider(format!("Cerebras request failed: {error}"))
            })?;

            {
                let snap = RateLimitSnapshot::from_headers(response.headers());
                if let Ok(mut slot) = rate_limit_slot.lock() {
                    *slot = Some(snap);
                }
            }

            let status = response.status();
            if !status.is_success() {
                let body_text = response.text().await.unwrap_or_default();
                return Err(LoopError::Provider(format!(
                    "Cerebras request failed with status {status}: {body_text}"
                )));
            }

            if config.streaming {
                Ok(CerebrasTurn {
                    inner: TurnInner::Streaming(Box::new(StreamingState {
                        body: response.bytes_stream(),
                        decoder: SseDecoder::new(),
                        translator: EventTranslator::new(),
                        pending: VecDeque::new(),
                        eof: false,
                    })),
                })
            } else {
                let body_text = response.text().await.map_err(|error| {
                    LoopError::Provider(format!("failed to read Cerebras response body: {error}"))
                })?;

                let events = response::build_turn_from_response(&body_text)
                    .map_err(|e| LoopError::Provider(e.to_string()))?;
                Ok(CerebrasTurn {
                    inner: TurnInner::Buffered { events },
                })
            }
        };

        if let Some(cancellation) = cancellation {
            futures_util::pin_mut!(request_future);
            let cancelled = cancellation.cancelled();
            futures_util::pin_mut!(cancelled);
            match select(request_future, cancelled).await {
                Either::Left((result, _)) => result,
                Either::Right((_, _)) => Err(LoopError::Cancelled),
            }
        } else {
            request_future.await
        }
    }

    fn model_name(&self) -> Option<&str> {
        Some(&self.config.model)
    }
}

fn attach_body(builder: HttpRequestBuilder, body: bytes::Bytes) -> HttpRequestBuilder {
    builder.body(body)
}

#[async_trait]
impl ModelTurn for CerebrasTurn {
    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
        {
            return Err(LoopError::Cancelled);
        }
        match &mut self.inner {
            TurnInner::Buffered { events } => Ok(events.pop_front()),
            TurnInner::Streaming(state) => {
                let StreamingState {
                    body,
                    decoder,
                    translator,
                    pending,
                    eof,
                } = state.as_mut();
                next_streaming_event(body, decoder, translator, pending, eof, cancellation).await
            }
        }
    }
}

async fn next_streaming_event(
    body: &mut BodyStream,
    decoder: &mut SseDecoder,
    translator: &mut EventTranslator,
    pending: &mut VecDeque<ModelTurnEvent>,
    eof: &mut bool,
    cancellation: Option<TurnCancellation>,
) -> Result<Option<ModelTurnEvent>, LoopError> {
    loop {
        if let Some(event) = pending.pop_front() {
            return Ok(Some(event));
        }
        if *eof || translator.is_done() {
            return Ok(None);
        }

        let chunk = if let Some(cancellation) = cancellation.as_ref() {
            let next = body.next();
            futures_util::pin_mut!(next);
            let cancelled = cancellation.cancelled();
            futures_util::pin_mut!(cancelled);
            match select(next, cancelled).await {
                Either::Left((chunk, _)) => chunk,
                Either::Right((_, _)) => return Err(LoopError::Cancelled),
            }
        } else {
            body.next().await
        };

        match chunk {
            Some(Ok(bytes)) => {
                let text = std::str::from_utf8(&bytes).map_err(|e| {
                    LoopError::Provider(format!("invalid UTF-8 in Cerebras stream: {e}"))
                })?;
                for sse in decoder.feed(text) {
                    match translator.handle(&sse) {
                        Ok(produced) => {
                            for ev in produced {
                                pending.push_back(ev);
                            }
                        }
                        Err(e) => return Err(LoopError::Provider(e.to_string())),
                    }
                }
            }
            Some(Err(e)) => {
                return Err(LoopError::Provider(format!(
                    "Cerebras stream body error: {e}"
                )));
            }
            None => {
                *eof = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{CancellationController, FinishReason};
    use agentkit_http::HttpError;
    use bytes::Bytes;
    use futures_util::stream;

    fn streaming_turn_from(chunks: Vec<&'static str>) -> CerebrasTurn {
        let body: BodyStream = Box::pin(stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, HttpError>(Bytes::from_static(c.as_bytes()))),
        ));
        CerebrasTurn {
            inner: TurnInner::Streaming(Box::new(StreamingState {
                body,
                decoder: SseDecoder::new(),
                translator: EventTranslator::new(),
                pending: VecDeque::new(),
                eof: false,
            })),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streaming_turn_drains_to_finished() {
        let chunks = vec![
            "data: {\"id\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"done\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        ];
        let mut turn = streaming_turn_from(chunks);
        let mut saw_finished = false;
        while let Some(event) = turn.next_event(None).await.expect("next_event") {
            if let ModelTurnEvent::Finished(result) = event {
                assert_eq!(result.finish_reason, FinishReason::Completed);
                saw_finished = true;
            }
        }
        assert!(saw_finished);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streaming_turn_respects_pre_fired_cancellation() {
        let chunks = vec!["data: {\"id\":\"m\",\"choices\":[]}\n\n"];
        let mut turn = streaming_turn_from(chunks);
        let controller = CancellationController::new();
        let checkpoint = TurnCancellation::new(controller.handle());
        controller.interrupt();
        let err = turn.next_event(Some(checkpoint)).await.unwrap_err();
        assert!(matches!(err, LoopError::Cancelled));
    }
}
