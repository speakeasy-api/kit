//! Anthropic Messages API adapter for the agentkit agent loop.
//!
//! This crate implements the agentkit [`ModelAdapter`] directly against
//! Anthropic's `/v1/messages` endpoint. The API is not OpenAI-compatible
//! (different message shape, `system` is top-level, tool results live as
//! content blocks inside user messages, etc.), so the generic completions
//! adapter is not reused.
//!
//! Streaming is on by default: the adapter consumes Anthropic's SSE response
//! and yields `ModelTurnEvent`s as tokens arrive. Call
//! [`AnthropicConfig::with_streaming(false)`] to opt out in favour of a single
//! buffered request.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use agentkit_loop::{Agent, SessionConfig};
//! use agentkit_provider_anthropic::{AnthropicAdapter, AnthropicConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = AnthropicConfig::from_env()?;
//!     let adapter = AnthropicAdapter::new(config)?;
//!     let agent = Agent::builder().model(adapter).build()?;
//!     let _driver = agent.start(SessionConfig::new("demo")).await?;
//!     Ok(())
//! }
//! ```

mod config;
mod error;
mod media;
mod request;
mod response;
mod server_tool;
mod sse;
mod stream;

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use agentkit_core::TurnCancellation;
use agentkit_http::{BodyStream, Http, HttpError, HttpRequestBuilder};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, SessionConfig,
    StructuredOutputCapability, StructuredOutputEvidence, StructuredOutputRequest, TurnRequest,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::future::{Either, select};

use crate::stream::{EventTranslator, SseDecoder};

pub use crate::config::{
    AnthropicConfig, AnthropicMcpServer, DEFAULT_ANTHROPIC_VERSION, DEFAULT_ENDPOINT, OutputEffort,
    OutputFormat, ServiceTier, ThinkingConfig, ToolChoice,
};
pub use crate::error::AnthropicError;
pub use crate::server_tool::{
    BashCodeExecutionTool, CodeExecutionTool, DEFAULT_BASH_EXECUTION_VERSION,
    DEFAULT_CODE_EXECUTION_VERSION, DEFAULT_TEXT_EDITOR_EXECUTION_VERSION,
    DEFAULT_WEB_FETCH_VERSION, DEFAULT_WEB_SEARCH_VERSION, RawServerTool, ServerTool,
    ServerToolHandle, TextEditorCodeExecutionTool, WebFetchTool, WebSearchTool, boxed,
};

/// Model adapter that connects the agentkit agent loop to the Anthropic
/// Messages API.
#[derive(Clone)]
pub struct AnthropicAdapter {
    client: Http,
    config: Arc<AnthropicConfig>,
}

impl AnthropicAdapter {
    /// Creates a new adapter from the given configuration, building a default
    /// reqwest-backed HTTP client.
    pub fn new(config: AnthropicConfig) -> Result<Self, AnthropicError> {
        let client = reqwest::Client::builder()
            .build()
            .map(Http::new)
            .map_err(|error| AnthropicError::HttpClient(HttpError::request(error)))?;
        Ok(Self {
            client,
            config: Arc::new(config),
        })
    }

    /// Creates a new adapter using a pre-configured [`Http`] client.
    pub fn with_client(config: AnthropicConfig, client: Http) -> Self {
        Self {
            client,
            config: Arc::new(config),
        }
    }

    /// Configured provider output-token ceiling.
    pub fn max_output_tokens(&self) -> u32 {
        self.config.max_tokens
    }
}

/// An active session with the Anthropic Messages API.
pub struct AnthropicSession {
    client: Http,
    config: Arc<AnthropicConfig>,
    _session_config: SessionConfig,
    structured_output: Option<StructuredOutputCapability>,
}

/// A turn in progress against the Messages API.
///
/// Either runs in buffered (full-JSON) or streaming (SSE) mode depending on
/// [`AnthropicConfig::streaming`]. The variant is private because the
/// streaming state carries opaque decoder/translator types.
pub struct AnthropicTurn {
    inner: TurnInner,
    structured_output: Option<StructuredOutputRequest>,
    structured_correlation: Option<(String, String)>,
    streamed_output_bytes: usize,
}

enum TurnInner {
    /// Buffered, non-streaming mode.
    Buffered { events: VecDeque<ModelTurnEvent> },
    /// Live SSE stream in progress. Boxed because [`EventTranslator`] carries
    /// a fairly large state machine and SSE responses are a small fraction of
    /// total turns; keeping the enum compact avoids a ~350B stack cost on the
    /// buffered path.
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
impl ModelAdapter for AnthropicAdapter {
    type Session = AnthropicSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(AnthropicSession {
            client: self.client.clone(),
            config: self.config.clone(),
            _session_config: config,
            structured_output: structured_output_capability(&self.config.model),
        })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("anthropic")
    }
}

#[async_trait]
impl ModelSession for AnthropicSession {
    type Turn = AnthropicTurn;

    async fn begin_turn(
        &mut self,
        turn_request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<AnthropicTurn, LoopError> {
        let config = self.config.clone();
        let structured_output = turn_request.structured_output.clone();
        if structured_output.is_some() && self.structured_output.is_none() {
            return Err(LoopError::Provider(format!(
                "Anthropic model {} has no sealed structured-output capability",
                self.config.model
            )));
        }
        if let Some(request) = &structured_output {
            validate_schema_subset(request.schema())?;
        }
        let structured_correlation = structured_output.as_ref().map(|_| {
            (
                turn_request.session_id.to_string(),
                turn_request.turn_id.to_string(),
            )
        });

        let request_future = async move {
            let body = request::build_request_body(&config, &turn_request)
                .map_err(|e| LoopError::Provider(e.to_string()))?;

            let betas = collect_beta_flags(&config);

            let mut http = self
                .client
                .post(&config.base_url)
                .header("Content-Type", "application/json")
                .header("anthropic-version", config.anthropic_version.as_str());

            http = attach_auth(http, &config)?;

            if !betas.is_empty() {
                let joined = betas.into_iter().collect::<Vec<_>>().join(",");
                http = http.header("anthropic-beta", joined);
            }

            http = http.header(
                "User-Agent",
                concat!("agentkit-provider-anthropic/", env!("CARGO_PKG_VERSION")),
            );

            if config.streaming {
                http = http.header("Accept", "text/event-stream");
            }

            let response = http.json(&body).send().await.map_err(|error| {
                LoopError::Provider(format!("Anthropic request failed: {error}"))
            })?;

            let status = response.status();

            if !status.is_success() {
                // Drain the body for the error message, regardless of mode —
                // the server typically returns JSON error details here.
                let body_text = read_bounded_text(response.bytes_stream(), 1024 * 1024)
                    .await
                    .unwrap_or_default();
                return Err(LoopError::Provider(format!(
                    "Anthropic request failed with status {status}: {body_text}"
                )));
            }

            if config.streaming {
                Ok(AnthropicTurn {
                    inner: TurnInner::Streaming(Box::new(StreamingState {
                        body: response.bytes_stream(),
                        decoder: SseDecoder::new(),
                        translator: EventTranslator::new(),
                        pending: VecDeque::new(),
                        eof: false,
                    })),
                    structured_output,
                    structured_correlation,
                    streamed_output_bytes: 0,
                })
            } else {
                let body_text = read_bounded_text(
                    response.bytes_stream(),
                    structured_output
                        .as_ref()
                        .map_or(64 * 1024 * 1024, StructuredOutputRequest::max_output_bytes),
                )
                .await?;

                let events = response::build_turn_from_response(&body_text)
                    .map_err(|e| LoopError::Provider(e.to_string()))?;
                Ok(AnthropicTurn {
                    inner: TurnInner::Buffered { events },
                    structured_output,
                    structured_correlation,
                    streamed_output_bytes: 0,
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

    fn structured_output_capability(&self) -> Option<&StructuredOutputCapability> {
        self.structured_output.as_ref()
    }
}

#[async_trait]
impl ModelTurn for AnthropicTurn {
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
        let event = match &mut self.inner {
            TurnInner::Buffered { events } => events.pop_front(),
            TurnInner::Streaming(state) => {
                let StreamingState {
                    body,
                    decoder,
                    translator,
                    pending,
                    eof,
                } = state.as_mut();
                next_streaming_event(body, decoder, translator, pending, eof, cancellation).await?
            }
        };
        if let Some(request) = &self.structured_output {
            let additional = match &event {
                Some(ModelTurnEvent::Delta(agentkit_core::Delta::AppendText { chunk, .. })) => {
                    chunk.len()
                }
                Some(ModelTurnEvent::Delta(agentkit_core::Delta::AppendBytes {
                    chunk, ..
                })) => chunk.len(),
                _ => 0,
            };
            self.streamed_output_bytes = self
                .streamed_output_bytes
                .checked_add(additional)
                .ok_or_else(|| {
                    LoopError::Provider("Anthropic output size overflowed".to_owned())
                })?;
            if self.streamed_output_bytes > request.max_output_bytes() {
                return Err(LoopError::Provider(format!(
                    "Anthropic structured output exceeds {} bytes",
                    request.max_output_bytes()
                )));
            }
        }
        match event {
            Some(ModelTurnEvent::Finished(mut result)) => {
                if let Some(request) = &self.structured_output {
                    let (session_id, turn_id) = self
                        .structured_correlation
                        .as_ref()
                        .expect("structured requests retain correlation");
                    project_structured_result(&mut result, request, session_id, turn_id)?;
                }
                Ok(Some(ModelTurnEvent::Finished(result)))
            }
            event => Ok(event),
        }
    }
}

async fn read_bounded_text(mut stream: BodyStream, maximum: usize) -> Result<String, LoopError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            LoopError::Provider(format!("failed to read Anthropic response body: {error}"))
        })?;
        let next = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| LoopError::Provider("Anthropic response size overflowed".to_owned()))?;
        if next > maximum {
            return Err(LoopError::Provider(format!(
                "Anthropic response exceeds {maximum} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|error| {
        LoopError::Provider(format!("invalid UTF-8 in Anthropic response: {error}"))
    })
}

fn project_structured_result(
    result: &mut agentkit_loop::ModelTurnResult,
    request: &StructuredOutputRequest,
    session_id: &str,
    turn_id: &str,
) -> Result<(), LoopError> {
    let mut structured = 0usize;
    for item in &mut result.output_items {
        for part in &mut item.parts {
            if let agentkit_core::Part::Text(text) = part {
                let value = match serde_json::from_str(&text.text) {
                    Ok(value) => value,
                    Err(error) => {
                        insert_structured_evidence(
                            result,
                            request,
                            session_id,
                            turn_id,
                            false,
                            Some(error.to_string()),
                        )?;
                        return Ok(());
                    }
                };
                if let Err(error) = jsonschema::draft202012::options()
                    .build(request.schema())
                    .and_then(|validator| validator.validate(&value))
                {
                    insert_structured_evidence(
                        result,
                        request,
                        session_id,
                        turn_id,
                        false,
                        Some(error.to_string()),
                    )?;
                    return Ok(());
                }
                *part = agentkit_core::Part::Structured(
                    agentkit_core::StructuredPart::new(value).with_schema(request.schema().clone()),
                );
                structured += 1;
            }
        }
    }
    if structured != 1 {
        insert_structured_evidence(
            result,
            request,
            session_id,
            turn_id,
            false,
            Some("expected exactly one JSON value".to_owned()),
        )?;
        return Ok(());
    }
    insert_structured_evidence(result, request, session_id, turn_id, true, None)?;
    Ok(())
}

fn insert_structured_evidence(
    result: &mut agentkit_loop::ModelTurnResult,
    request: &StructuredOutputRequest,
    session_id: &str,
    turn_id: &str,
    honored: bool,
    error: Option<String>,
) -> Result<(), LoopError> {
    result.metadata.insert(
        "agentkit.structured_output".to_owned(),
        serde_json::to_value(StructuredOutputEvidence {
            name: request.name().to_owned(),
            version: request.version(),
            strict: request.strict(),
            schema_digest: request.schema_digest().to_owned(),
            session_id: session_id.to_owned(),
            turn_id: turn_id.to_owned(),
            honored,
            error,
        })
        .map_err(|error| LoopError::Provider(error.to_string()))?,
    );
    Ok(())
}

const STRUCTURED_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
const STRUCTURED_KEYWORDS: &[&str] = &[
    "$id",
    "$schema",
    "additionalProperties",
    "const",
    "enum",
    "items",
    "maxItems",
    "minLength",
    "minimum",
    "oneOf",
    "pattern",
    "properties",
    "required",
    "type",
];

struct StructuredCapabilityCell {
    model: &'static str,
    adapter_version: &'static str,
    dialect: &'static str,
    keywords: &'static [&'static str],
}

#[cfg(debug_assertions)]
const STRUCTURED_CAPABILITY_CELLS: &[StructuredCapabilityCell] = &[StructuredCapabilityCell {
    model: "kit-anthropic-structured-fixture-v1",
    adapter_version: env!("CARGO_PKG_VERSION"),
    dialect: STRUCTURED_DIALECT,
    keywords: STRUCTURED_KEYWORDS,
}];

#[cfg(not(debug_assertions))]
const STRUCTURED_CAPABILITY_CELLS: &[StructuredCapabilityCell] = &[];

fn structured_output_capability(model: &str) -> Option<StructuredOutputCapability> {
    let cell = STRUCTURED_CAPABILITY_CELLS.iter().find(|cell| {
        cell.model == model
            && cell.adapter_version == env!("CARGO_PKG_VERSION")
            && cell.dialect == STRUCTURED_DIALECT
            && cell.keywords == STRUCTURED_KEYWORDS
    })?;
    StructuredOutputCapability::new(
        format!(
            "anthropic.output-format.v1;adapter={};dialect=draft2020-12;keywords={}",
            cell.adapter_version,
            cell.keywords.join(",")
        ),
        true,
        64 * 1024,
    )
}

fn validate_schema_subset(schema: &serde_json::Value) -> Result<(), LoopError> {
    fn visit(value: &serde_json::Value, property_names: bool) -> Result<(), LoopError> {
        match value {
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    if !property_names && !STRUCTURED_KEYWORDS.contains(&key.as_str()) {
                        return Err(LoopError::Provider(format!(
                            "unsupported Anthropic structured-output schema keyword {key}"
                        )));
                    }
                    visit(value, key == "properties")?;
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, false)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    if schema.get("$schema").and_then(serde_json::Value::as_str) != Some(STRUCTURED_DIALECT) {
        return Err(LoopError::Provider(
            "unsupported Anthropic structured-output schema dialect".to_owned(),
        ));
    }
    visit(schema, false)?;
    jsonschema::draft202012::options()
        .build(schema)
        .map(|_| ())
        .map_err(|error| LoopError::Provider(format!("invalid structured-output schema: {error}")))
}

/// Pulls the next event from an active SSE stream, decoding more bytes as
/// needed. Returns `Ok(None)` once the translator has emitted `Finished` and
/// the pending queue is empty.
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

        // Await the next chunk, racing against cancellation so long-lived
        // streams can be interrupted mid-response.
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
                    LoopError::Provider(format!("invalid UTF-8 in Anthropic stream: {e}"))
                })?;
                for sse in decoder.feed(text) {
                    for produced in translator.handle(&sse)? {
                        pending.push_back(produced);
                    }
                }
            }
            Some(Err(e)) => {
                return Err(LoopError::Provider(format!(
                    "Anthropic stream body error: {e}"
                )));
            }
            None => {
                *eof = true;
            }
        }
    }
}

fn attach_auth(
    builder: HttpRequestBuilder,
    config: &AnthropicConfig,
) -> Result<HttpRequestBuilder, LoopError> {
    if let Some(token) = &config.auth_token {
        return Ok(builder.bearer_auth(token));
    }
    if let Some(key) = &config.api_key {
        return Ok(builder.header("x-api-key", key.as_str()));
    }
    Err(LoopError::Provider(
        AnthropicError::MissingCredentials.to_string(),
    ))
}

fn collect_beta_flags(config: &AnthropicConfig) -> BTreeSet<String> {
    let mut betas: BTreeSet<String> = config.anthropic_beta.iter().cloned().collect();
    for tool in &config.server_tools {
        for flag in tool.beta_flags() {
            betas.insert(flag);
        }
    }
    betas
}

#[cfg(test)]
mod tests {
    use agentkit_core::{CancellationController, FinishReason};
    use agentkit_http::HttpError;
    use bytes::Bytes;
    use futures_util::stream;

    use super::*;

    #[test]
    fn rejects_zero_max_tokens() {
        match AnthropicConfig::new("k", "claude-opus-4-7", 0) {
            Err(AnthropicError::InvalidMaxTokens) => {}
            other => panic!("expected InvalidMaxTokens, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn beta_flags_union_includes_server_tool_requirements() {
        let cfg = AnthropicConfig::new("k", "claude-opus-4-7", 1024)
            .unwrap()
            .with_beta("extended-thinking-2025-05-07")
            .with_server_tool(boxed(
                RawServerTool::new(serde_json::json!({
                    "type": "future_tool_20271231",
                    "name": "future_tool",
                }))
                .with_beta("future-tool-2027-12-31"),
            ));
        let flags = collect_beta_flags(&cfg);
        assert!(flags.contains("extended-thinking-2025-05-07"));
        assert!(flags.contains("future-tool-2027-12-31"));
    }

    /// Builds an `AnthropicTurn::Streaming` backed by a canned byte stream so
    /// we can exercise the full decode -> translate -> yield pipeline without
    /// a live HTTP connection.
    fn streaming_turn_from(chunks: Vec<&'static str>) -> AnthropicTurn {
        let body: BodyStream = Box::pin(stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, HttpError>(Bytes::from_static(c.as_bytes()))),
        ));
        AnthropicTurn {
            inner: TurnInner::Streaming(Box::new(StreamingState {
                body,
                decoder: SseDecoder::new(),
                translator: EventTranslator::new(),
                pending: VecDeque::new(),
                eof: false,
            })),
            structured_output: None,
            structured_correlation: None,
            streamed_output_bytes: 0,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streaming_turn_drains_to_finished() {
        let chunks = vec![
            "event: message_start\ndata: {\"message\":{\"id\":\"m\",\"model\":\"x\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"index\":0}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {}\n\n",
        ];
        let mut turn = streaming_turn_from(chunks);

        let mut seen_finished = false;
        while let Some(event) = turn.next_event(None).await.expect("next_event") {
            if let ModelTurnEvent::Finished(result) = event {
                assert_eq!(result.finish_reason, FinishReason::Completed);
                seen_finished = true;
            }
        }
        assert!(seen_finished, "turn never emitted Finished");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streaming_turn_respects_pre_fired_cancellation() {
        let chunks = vec![
            "event: message_start\ndata: {\"message\":{\"id\":\"m\",\"model\":\"x\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        ];
        let mut turn = streaming_turn_from(chunks);

        let controller = CancellationController::new();
        let checkpoint = TurnCancellation::new(controller.handle());
        // Fire cancellation before polling.
        controller.interrupt();

        let err = turn.next_event(Some(checkpoint)).await.unwrap_err();
        assert!(matches!(err, LoopError::Cancelled));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn buffered_body_is_bounded_before_text_or_json_parsing() {
        let body = |chunks: Vec<&'static [u8]>| -> BodyStream {
            Box::pin(stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok::<_, HttpError>(Bytes::from_static(chunk))),
            ))
        };
        assert_eq!(
            read_bounded_text(body(vec![b"ab", b"cd"]), 4)
                .await
                .unwrap(),
            "abcd"
        );
        assert!(
            read_bounded_text(body(vec![b"ab", b"cd"]), 3)
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds 3 bytes")
        );
    }

    #[test]
    fn malformed_structured_text_reaches_terminal_classifier_unclaimed() {
        let request =
            StructuredOutputRequest::new("edit", 1, true, serde_json::json!({"type": "object"}))
                .unwrap();
        let mut result = agentkit_loop::ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![agentkit_core::Item::new(
                agentkit_core::ItemKind::Assistant,
                vec![agentkit_core::Part::text("{")],
            )],
            usage: None,
            metadata: Default::default(),
            model: None,
            response_id: None,
        };
        project_structured_result(&mut result, &request, "session", "turn").unwrap();
        assert_eq!(
            result.metadata["agentkit.structured_output"]["honored"],
            false
        );
        assert!(matches!(
            result.output_items[0].parts[0],
            agentkit_core::Part::Text(_)
        ));
    }

    #[test]
    fn structured_capability_uses_only_exact_sealed_cells() {
        assert!(structured_output_capability("unknown-model").is_none());
        assert_eq!(
            structured_output_capability("kit-anthropic-structured-fixture-v1").is_some(),
            cfg!(debug_assertions)
        );
    }

    #[test]
    fn schema_invalid_json_is_not_honored_or_projected() {
        let request = StructuredOutputRequest::new(
            "edit",
            1,
            true,
            serde_json::json!({
                "$schema": STRUCTURED_DIALECT,
                "additionalProperties": false,
                "properties": {"version": {"const": 1}},
                "required": ["version"],
                "type": "object"
            }),
        )
        .unwrap();
        let mut result = agentkit_loop::ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![agentkit_core::Item::new(
                agentkit_core::ItemKind::Assistant,
                vec![agentkit_core::Part::text("{}")],
            )],
            usage: None,
            metadata: Default::default(),
            model: None,
            response_id: None,
        };
        project_structured_result(&mut result, &request, "session", "turn").unwrap();
        let evidence: StructuredOutputEvidence =
            serde_json::from_value(result.metadata["agentkit.structured_output"].clone()).unwrap();
        assert!(!evidence.honored);
        assert_eq!(evidence.session_id, "session");
        assert_eq!(evidence.turn_id, "turn");
        assert!(matches!(
            result.output_items[0].parts[0],
            agentkit_core::Part::Text(_)
        ));
    }
}
