use std::{
    collections::{HashMap, HashSet, VecDeque},
    pin::Pin,
    time::{Duration, Instant},
};

use agentkit_core::{
    Delta, FinishReason, Item, ItemKind, MetadataMap, Part, PartId, PartKind, ReasoningPart,
    TextPart, TokenUsage, ToolCallPart, ToolOutput, Usage,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    PromptCacheMode, PromptCacheStrategy, SessionConfig, TurnRequest,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use serde_json::{Value, json};
use zeroize::{Zeroize, Zeroizing};

use crate::cli::auth;

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_ITEMS: usize = 10_000;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const PROVIDER_REQUEST_DIGEST: &str = "kit.model_call.request_digest";
pub(crate) const CONTINUATION_METADATA: &str = "openai.subscription.v1";
const CONTINUATION_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Debug)]
pub(crate) struct SubscriptionConfig {
    pub model: String,
    #[cfg(test)]
    endpoint: String,
    #[cfg(test)]
    credentials: Option<std::sync::Arc<std::sync::Mutex<VecDeque<crate::cli::auth::TokenRecord>>>>,
}

impl SubscriptionConfig {
    pub(crate) fn new(model: String) -> Result<Self, String> {
        if !supported_model(&model) {
            return Err("openai-subscription model is not in the supported model set".to_owned());
        }
        Ok(Self {
            model,
            #[cfg(test)]
            endpoint: ENDPOINT.to_owned(),
            #[cfg(test)]
            credentials: None,
        })
    }

    fn endpoint(&self) -> &str {
        #[cfg(test)]
        return &self.endpoint;
        #[cfg(not(test))]
        return ENDPOINT;
    }

    #[cfg(test)]
    pub(crate) fn with_test_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = endpoint;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_credentials(
        mut self,
        credentials: Vec<crate::cli::auth::TokenRecord>,
    ) -> Self {
        self.credentials = Some(std::sync::Arc::new(std::sync::Mutex::new(
            credentials.into(),
        )));
        self
    }
}

pub(crate) fn supported_model(model: &str) -> bool {
    matches!(
        model,
        "gpt-5.6-sol" | "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.3-codex-spark"
    )
}

#[derive(Clone)]
pub(crate) struct OpenAiSubscriptionAdapter {
    config: SubscriptionConfig,
    client: reqwest::Client,
}

impl OpenAiSubscriptionAdapter {
    pub(crate) fn new(config: SubscriptionConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "could not build openai-subscription TLS client".to_owned())?;
        Ok(Self { config, client })
    }
}

#[async_trait]
impl ModelAdapter for OpenAiSubscriptionAdapter {
    type Session = OpenAiSubscriptionSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        let session_id = config.session_id.to_string();
        if session_id.is_empty() || session_id.len() > 256 || !session_id.is_ascii() {
            return Err(protocol("session ID is outside canonical bounds"));
        }
        let credentials = credentials(&self.config, None, None).await?;
        let binding = credentials
            .binding()
            .map_err(|error| LoopError::Provider(error.to_string()))?;
        Ok(OpenAiSubscriptionSession {
            config: self.config.clone(),
            client: self.client.clone(),
            session_id,
            binding,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("openai-subscription")
    }
}

pub(crate) struct OpenAiSubscriptionSession {
    config: SubscriptionConfig,
    client: reqwest::Client,
    session_id: String,
    binding: auth::CredentialBinding,
}

#[async_trait]
impl ModelSession for OpenAiSubscriptionSession {
    type Turn = OpenAiSubscriptionTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        if cancellation
            .as_ref()
            .is_some_and(|value| value.is_cancelled())
        {
            return Err(LoopError::Cancelled);
        }
        let mut credentials = self.credentials(None, cancellation.clone()).await?;
        self.ensure_binding(&credentials)?;
        let idempotency_key = request_idempotency_key(&request)?;
        let mut body = request_body(
            &self.config.model,
            &request,
            &self.binding,
            &self.session_id,
        )?;
        let encoded = serde_json::to_vec(&body).map(Zeroizing::new);
        zeroize_encrypted_content(&mut body);
        let body_bytes = encoded.map_err(|_| protocol("request encoding failed"))?;
        if body_bytes.len() > MAX_REQUEST_BYTES {
            return Err(protocol("request exceeds 8 MiB"));
        }
        let mut unauthorized = false;
        let mut retry = false;
        let response = loop {
            self.ensure_binding(&credentials)?;
            let mut builder = self
                .client
                .post(self.config.endpoint())
                .bearer_auth(credentials.access_token())
                .header("originator", "kit")
                .header("session-id", &self.session_id)
                .header("Idempotency-Key", &idempotency_key)
                .header("Accept", "text/event-stream")
                .header("Content-Type", "application/json");
            if let Some(account_id) = credentials.account_id() {
                builder = builder.header("ChatGPT-Account-ID", account_id);
            }
            let send = builder.body(body_bytes.to_vec()).send();
            let response = if let Some(cancel) = cancellation.clone() {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(LoopError::Cancelled),
                    response = send => response,
                }
            } else {
                send.await
            }
            .map_err(|_| LoopError::Provider("openai-subscription transport failed".to_owned()))?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !unauthorized {
                unauthorized = true;
                credentials = self
                    .credentials(Some(credentials), cancellation.clone())
                    .await?;
                self.ensure_binding(&credentials)?;
                continue;
            }
            if known_not_dispatched(response.status()) {
                let delay = retry_after(response.headers());
                if !retry {
                    retry = true;
                    if let Some(delay) = delay {
                        if let Some(cancel) = cancellation.clone() {
                            tokio::select! {
                                _ = cancel.cancelled() => return Err(LoopError::Cancelled),
                                _ = tokio::time::sleep(delay) => {}
                            }
                        } else {
                            tokio::time::sleep(delay).await;
                        }
                    }
                    credentials = self.credentials(None, cancellation.clone()).await?;
                    continue;
                }
                return Err(LoopError::Provider(format!(
                    "openai-subscription returned {} after one internal retry",
                    response.status()
                )));
            }
            break response;
        };
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(LoopError::Provider(
                "openai-subscription unauthorized after one refresh".to_owned(),
            ));
        }
        if !status.is_success() {
            let detail = failure_body_excerpt(response).await;
            return Err(LoopError::Provider(format!(
                "openai-subscription returned {status}{detail}"
            )));
        }
        // The codex backend omits Content-Type on successful SSE responses, so absence
        // is accepted; only an explicitly different declared type is rejected. The SSE
        // parser remains fail-closed on malformed bodies.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        if content_type.is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_none_or(|value| value.trim() != "text/event-stream")
        }) {
            return Err(protocol("response is not an SSE stream"));
        }
        let response_model = response
            .headers()
            .get("openai-model")
            .map(validated_model_header)
            .transpose()?;
        let turn = OpenAiSubscriptionTurn::new_inner(
            response.bytes_stream(),
            self.config.model.clone(),
            response_model,
            self.binding.clone(),
            self.session_id.clone(),
        );
        Ok(turn)
    }

    fn model_name(&self) -> Option<&str> {
        Some(&self.config.model)
    }
}

impl OpenAiSubscriptionSession {
    fn ensure_binding(&self, credentials: &auth::TokenRecord) -> Result<(), LoopError> {
        let binding = credentials
            .binding()
            .map_err(|error| LoopError::SessionStale(error.to_string()))?;
        if binding == self.binding {
            Ok(())
        } else {
            Err(LoopError::SessionStale(
                "OpenAI credential account or generation changed".to_owned(),
            ))
        }
    }

    async fn credentials(
        &self,
        rejected: Option<auth::TokenRecord>,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<auth::TokenRecord, LoopError> {
        credentials(&self.config, rejected, cancellation).await
    }
}

async fn credentials(
    config: &SubscriptionConfig,
    rejected: Option<auth::TokenRecord>,
    cancellation: Option<agentkit_core::TurnCancellation>,
) -> Result<auth::TokenRecord, LoopError> {
    #[cfg(not(test))]
    let _ = config;
    #[cfg(test)]
    if let Some(credentials) = &config.credentials {
        let mut credentials = credentials
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if rejected.is_some() {
            credentials.pop_front();
        }
        return credentials
            .front()
            .cloned()
            .ok_or_else(|| LoopError::Provider("test credentials exhausted".to_owned()));
    }
    let worker = tokio::task::spawn_blocking(move || match rejected {
        Some(record) => auth::refresh_after_unauthorized(
            record.access_token(),
            Instant::now() + Duration::from_secs(30),
        ),
        None => auth::access_token(Instant::now() + Duration::from_secs(30)),
    });
    let result = if let Some(cancel) = cancellation {
        tokio::select! {
            _ = cancel.cancelled() => return Err(LoopError::Cancelled),
            result = worker => result,
        }
    } else {
        worker.await
    };
    result
        .map_err(|_| LoopError::Provider("openai-subscription auth worker failed".to_owned()))?
        .map_err(|error| LoopError::Provider(error.to_string()))
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

pub(crate) struct OpenAiSubscriptionTurn {
    stream: ByteStream,
    buffer: Zeroizing<Vec<u8>>,
    queued: VecDeque<ModelTurnEvent>,
    output: Vec<Item>,
    output_indices: Vec<u64>,
    seen_ids: HashSet<String>,
    seen_call_ids: HashSet<String>,
    done_ids: HashSet<String>,
    item_indices: HashMap<String, u64>,
    text: HashMap<(String, u64), PartAccumulator>,
    created: bool,
    completed: bool,
    sequence: Option<u64>,
    total_bytes: usize,
    usage: Option<Usage>,
    response_id: Option<String>,
    requested_model: String,
    header_model: Option<String>,
    response_model: Option<String>,
    binding: auth::CredentialBinding,
    session_id: String,
    tool_call: bool,
    stream_ended: bool,
}

struct PartAccumulator {
    id: PartId,
    text: String,
}

impl OpenAiSubscriptionTurn {
    #[cfg(test)]
    fn new(
        stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        requested_model: String,
        actual_model: Option<String>,
    ) -> Self {
        Self::new_inner(
            stream,
            requested_model,
            actual_model,
            auth::CredentialBinding {
                account_id: "test-account".to_owned(),
                generation: "test-generation".to_owned(),
            },
            "s".to_owned(),
        )
    }

    fn new_inner(
        stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        requested_model: String,
        header_model: Option<String>,
        binding: auth::CredentialBinding,
        session_id: String,
    ) -> Self {
        Self {
            stream: Box::pin(stream),
            buffer: Zeroizing::new(Vec::new()),
            queued: VecDeque::new(),
            output: Vec::new(),
            output_indices: Vec::new(),
            seen_ids: HashSet::new(),
            seen_call_ids: HashSet::new(),
            done_ids: HashSet::new(),
            item_indices: HashMap::new(),
            text: HashMap::new(),
            created: false,
            completed: false,
            sequence: None,
            total_bytes: 0,
            usage: None,
            response_id: None,
            requested_model,
            header_model,
            response_model: None,
            binding,
            session_id,
            tool_call: false,
            stream_ended: false,
        }
    }

    fn consume_frame(&mut self, frame: &[u8]) -> Result<(), LoopError> {
        if frame.len() > MAX_EVENT_BYTES {
            return Err(protocol("SSE event exceeds 1 MiB"));
        }
        let text = std::str::from_utf8(frame).map_err(|_| protocol("SSE event is not UTF-8"))?;
        let text = Zeroizing::new(text.replace("\r\n", "\n").replace('\r', "\n"));
        let mut data = Vec::new();
        let mut event_name = None;
        for line in text.split('\n') {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .map(|(name, value)| (name, value.strip_prefix(' ').unwrap_or(value)))
                .unwrap_or((line, ""));
            match name {
                "event" => event_name = Some(value),
                "data" => data.push(value),
                "id" | "retry" => {}
                _ => {}
            }
        }
        if data.is_empty() {
            return Ok(());
        }
        let data = Zeroizing::new(data.join("\n"));
        if data.as_str() == "[DONE]" {
            return Err(protocol("stream used an unsupported terminal marker"));
        }
        let mut value: Value = serde_json::from_str(data.as_str())
            .map_err(|_| protocol("SSE data is malformed JSON"))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("SSE event omitted type"))?;
        if event_name.is_some_and(|name| name != kind) {
            return Err(protocol("SSE event name/type mismatch"));
        }
        if let Some(sequence) = value.get("sequence_number").and_then(Value::as_u64) {
            let expected = self
                .sequence
                .map_or(sequence, |last| last.saturating_add(1));
            if sequence != expected {
                return Err(protocol("SSE sequence is duplicate or out of order"));
            }
            self.sequence = Some(sequence);
        }
        let result = self.consume_value(kind, &value);
        zeroize_encrypted_content(&mut value);
        result
    }

    fn consume_value(&mut self, kind: &str, value: &Value) -> Result<(), LoopError> {
        if self.completed {
            return Err(protocol("event followed response.completed"));
        }
        match kind {
            "response.created" => {
                if self.created || value.get("response").is_none() {
                    return Err(protocol("duplicate or malformed response.created"));
                }
                self.created = true;
                if let Some(model) = value.pointer("/response/model") {
                    self.observe_model(model)?;
                }
            }
            "response.output_text.delta" => {
                self.require_created()?;
                let delta = bounded_string(value, "delta")?;
                let item = event_item(value, "content_index", &self.item_indices)?;
                append_part(
                    &mut self.text,
                    item,
                    delta,
                    PartKind::Text,
                    &mut self.queued,
                )?;
            }
            "response.reasoning_summary_text.delta" => {
                self.require_created()?;
                bounded_string(value, "delta")?;
                event_item(value, "summary_index", &self.item_indices)?;
            }
            "response.output_item.added" => {
                self.require_created()?;
                let id = bounded_id(value.pointer("/item/id"))?;
                let index = nonnegative(value, "output_index")?;
                if !self.seen_ids.insert(id.to_owned()) {
                    return Err(protocol("duplicate output item ID"));
                }
                if self.item_indices.values().any(|value| *value == index) {
                    return Err(protocol("duplicate output item index"));
                }
                self.item_indices.insert(id.to_owned(), index);
            }
            "response.output_item.done" => {
                self.require_created()?;
                let item = value
                    .get("item")
                    .ok_or_else(|| protocol("output item omitted item"))?;
                let id = bounded_id(item.get("id"))?;
                if !self.seen_ids.contains(id) || !self.done_ids.insert(id.to_owned()) {
                    return Err(protocol(
                        "output item completed without add or completed twice",
                    ));
                }
                if value.get("output_index").and_then(Value::as_u64)
                    != self.item_indices.get(id).copied()
                {
                    return Err(protocol("completed output item index changed"));
                }
                self.output_item(
                    self.item_indices
                        .get(id)
                        .copied()
                        .expect("completed item has an index"),
                    item,
                )?;
            }
            "response.completed" => self.complete(value)?,
            "response.incomplete" => self.incomplete(value)?,
            "response.failed" => {
                self.output.retain(|item| {
                    item.parts
                        .iter()
                        .all(|part| !matches!(part, Part::ToolCall(_)))
                });
                return Err(classify_response_failure(value));
            }
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.in_progress"
            | "response.metadata" => {
                self.require_created()?;
            }
            _ => return Err(protocol(&format!("unknown Responses SSE event: {kind}"))),
        }
        Ok(())
    }

    fn output_item(&mut self, output_index: u64, item: &Value) -> Result<(), LoopError> {
        if self.output.len() >= MAX_ITEMS {
            return Err(protocol("too many output items"));
        }
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if item.get("role").and_then(Value::as_str) != Some("assistant") {
                    return Err(protocol("output message role is not assistant"));
                }
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| protocol("output message content is malformed"))?;
                let mut parts = Vec::new();
                let item_id = bounded_id(item.get("id"))?;
                for (index, part) in content.iter().enumerate() {
                    if part.get("type").and_then(Value::as_str) != Some("output_text") {
                        return Err(protocol("unsupported output message content"));
                    }
                    let text = bounded_string(part, "text")?;
                    self.commit_part(item_id, index as u64, text, Part::Text(TextPart::new(text)))?;
                    parts.push(Part::Text(TextPart::new(text)));
                }
                if self.text.keys().any(|(id, _)| id == item_id) {
                    return Err(protocol("completed output omitted streamed text content"));
                }
                self.push_output(output_index, Item::new(ItemKind::Assistant, parts));
            }
            Some("function_call") => {
                let call_id = bounded_string(item, "call_id")?;
                if !self.seen_call_ids.insert(call_id.to_owned()) {
                    return Err(protocol("duplicate function call ID"));
                }
                let name = bounded_string(item, "name")?;
                let arguments = bounded_string(item, "arguments")?;
                let input: Value = serde_json::from_str(arguments)
                    .map_err(|_| protocol("function-call arguments are not JSON"))?;
                if !input.is_object() {
                    return Err(protocol("function-call arguments are not an object"));
                }
                let call =
                    ToolCallPart::new(call_id, name, input).with_metadata(continuation_metadata(
                        &self.binding,
                        &self.requested_model,
                        &self.session_id,
                        bounded_id(item.get("id"))?,
                        output_index,
                        "function_call",
                        None,
                    ));
                self.tool_call = true;
                self.push_output(
                    output_index,
                    Item::new(ItemKind::Assistant, vec![Part::ToolCall(call)]),
                );
            }
            Some("reasoning") => {
                let summaries = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .ok_or_else(|| protocol("reasoning summary is malformed"))?;
                let encrypted_content = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= MAX_TEXT_BYTES)
                    .ok_or_else(|| {
                        protocol("encrypted reasoning is missing or outside canonical bounds")
                    })?;
                for summary in summaries {
                    bounded_string(summary, "text")?;
                }
                let metadata = continuation_metadata(
                    &self.binding,
                    &self.requested_model,
                    &self.session_id,
                    bounded_id(item.get("id"))?,
                    output_index,
                    "reasoning",
                    Some(encrypted_content),
                );
                self.push_output(
                    output_index,
                    Item::new(
                        ItemKind::Assistant,
                        vec![Part::Reasoning(ReasoningPart {
                            summary: None,
                            data: None,
                            redacted: true,
                            metadata,
                        })],
                    ),
                );
            }
            _ => return Err(protocol("unsupported Responses output item")),
        }
        Ok(())
    }

    fn complete(&mut self, value: &Value) -> Result<(), LoopError> {
        self.require_created()?;
        if self.seen_ids != self.done_ids || !self.text.is_empty() {
            return Err(protocol(
                "response.completed preceded complete output items",
            ));
        }
        let response = value
            .get("response")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol("response.completed omitted response"))?;
        let id = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or_else(|| protocol("response.completed omitted response ID"))?;
        self.response_id = Some(id.to_owned());
        self.finalize_continuation_metadata(id)?;
        if let Some(model) = response.get("model") {
            self.observe_model(model)?;
        }
        if let Some(raw) = response.get("usage") {
            let total_input = nonnegative(raw, "input_tokens")?;
            let total_output = nonnegative(raw, "output_tokens")?;
            let cached = optional_usage(raw.pointer("/input_tokens_details/cached_tokens"))?;
            let cache_write =
                optional_usage(raw.pointer("/input_tokens_details/cache_write_tokens"))?;
            let reasoning = optional_usage(raw.pointer("/output_tokens_details/reasoning_tokens"))?;
            let input = total_input
                .checked_sub(cached.unwrap_or_default())
                .and_then(|value| value.checked_sub(cache_write.unwrap_or_default()))
                .ok_or_else(|| {
                    protocol("nested input token categories exceed total input tokens")
                })?;
            let output = total_output
                .checked_sub(reasoning.unwrap_or_default())
                .ok_or_else(|| protocol("reasoning tokens exceed total output tokens"))?;
            let mut tokens = TokenUsage::new(input, output);
            if let Some(cached) = cached {
                tokens = tokens.with_cached_input_tokens(cached);
            }
            if let Some(cache_write) = cache_write {
                tokens = tokens.with_cache_write_input_tokens(cache_write);
            }
            if let Some(reasoning) = reasoning {
                tokens = tokens.with_reasoning_tokens(reasoning);
            }
            let usage = Usage::new(tokens);
            self.usage = Some(usage.clone());
            self.queued.push_back(ModelTurnEvent::Usage(usage));
        }
        self.completed = true;
        for call in self
            .output
            .iter()
            .flat_map(|item| item.parts.iter())
            .filter_map(|part| {
                if let Part::ToolCall(call) = part {
                    Some(call.clone())
                } else {
                    None
                }
            })
        {
            self.queued.push_back(ModelTurnEvent::ToolCall(call));
        }
        let metadata = model_metadata(self.header_model.as_deref(), self.response_model.as_deref());
        self.queued
            .push_back(ModelTurnEvent::Finished(ModelTurnResult {
                finish_reason: if self.tool_call {
                    FinishReason::ToolCall
                } else {
                    FinishReason::Completed
                },
                output_items: std::mem::take(&mut self.output),
                usage: self.usage.clone(),
                metadata,
                model: self
                    .header_model
                    .clone()
                    .or_else(|| self.response_model.clone()),
                response_id: self.response_id.clone(),
            }));
        Ok(())
    }

    fn incomplete(&mut self, value: &Value) -> Result<(), LoopError> {
        self.require_created()?;
        let response = value
            .get("response")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol("response.incomplete omitted response"))?;
        if let Some(id) = response.get("id") {
            self.response_id = Some(bounded_id(Some(id))?.to_owned());
        }
        if let Some(model) = response.get("model") {
            self.observe_model(model)?;
        }
        if let Some(raw) = response.get("usage") {
            let total_input = nonnegative(raw, "input_tokens")?;
            let total_output = nonnegative(raw, "output_tokens")?;
            let cached = optional_usage(raw.pointer("/input_tokens_details/cached_tokens"))?;
            let cache_write =
                optional_usage(raw.pointer("/input_tokens_details/cache_write_tokens"))?;
            let reasoning = optional_usage(raw.pointer("/output_tokens_details/reasoning_tokens"))?;
            let input = total_input
                .checked_sub(cached.unwrap_or_default())
                .and_then(|value| value.checked_sub(cache_write.unwrap_or_default()))
                .ok_or_else(|| {
                    protocol("nested input token categories exceed total input tokens")
                })?;
            let output = total_output
                .checked_sub(reasoning.unwrap_or_default())
                .ok_or_else(|| protocol("reasoning tokens exceed total output tokens"))?;
            let mut tokens = TokenUsage::new(input, output);
            if let Some(cached) = cached {
                tokens = tokens.with_cached_input_tokens(cached);
            }
            if let Some(cache_write) = cache_write {
                tokens = tokens.with_cache_write_input_tokens(cache_write);
            }
            if let Some(reasoning) = reasoning {
                tokens = tokens.with_reasoning_tokens(reasoning);
            }
            let usage = Usage::new(tokens);
            self.usage = Some(usage.clone());
            self.queued.push_back(ModelTurnEvent::Usage(usage));
        }
        self.flush_partial_output();
        for item in &mut self.output {
            item.parts.retain(|part| matches!(part, Part::Text(_)));
        }
        self.output.retain(|item| !item.parts.is_empty());
        let reason = response
            .get("incomplete_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty() && reason.len() <= 128 && reason.is_ascii())
            .ok_or_else(|| protocol("response.incomplete omitted a valid reason"))?;
        let finish_reason = match reason {
            "max_output_tokens" => FinishReason::MaxTokens,
            "content_filter" => FinishReason::Blocked,
            _ => return Err(protocol("unsupported response.incomplete reason")),
        };
        self.completed = true;
        let metadata = model_metadata(self.header_model.as_deref(), self.response_model.as_deref());
        self.queued
            .push_back(ModelTurnEvent::Finished(ModelTurnResult {
                finish_reason,
                output_items: std::mem::take(&mut self.output),
                usage: self.usage.clone(),
                metadata,
                model: self
                    .header_model
                    .clone()
                    .or_else(|| self.response_model.clone()),
                response_id: self.response_id.clone(),
            }));
        Ok(())
    }

    fn flush_partial_output(&mut self) {
        let mut partial = self
            .text
            .drain()
            .map(|((id, index), part)| (id, index, false, part))
            .collect::<Vec<_>>();
        partial.sort_by_key(|(id, index, reasoning, _)| {
            (
                self.item_indices.get(id).copied().unwrap_or(u64::MAX),
                *index,
                *reasoning,
            )
        });
        for (id, _, _, part) in partial {
            let output = Part::Text(TextPart::new(part.text));
            self.queued
                .push_back(ModelTurnEvent::Delta(Delta::CommitPart {
                    part: output.clone(),
                }));
            self.push_output(
                self.item_indices.get(&id).copied().unwrap_or(u64::MAX),
                Item::new(ItemKind::Assistant, vec![output]),
            );
        }
    }

    fn push_output(&mut self, output_index: u64, item: Item) {
        let position = self
            .output_indices
            .partition_point(|index| *index < output_index);
        self.output_indices.insert(position, output_index);
        self.output.insert(position, item);
    }

    fn require_created(&self) -> Result<(), LoopError> {
        if self.created {
            Ok(())
        } else {
            Err(protocol("response event preceded response.created"))
        }
    }

    fn finalize_continuation_metadata(&mut self, response_id: &str) -> Result<(), LoopError> {
        for part in self
            .output
            .iter_mut()
            .flat_map(|item| item.parts.iter_mut())
        {
            let metadata = match part {
                Part::Reasoning(part) => &mut part.metadata,
                Part::ToolCall(part) => &mut part.metadata,
                _ => continue,
            };
            let continuation = metadata
                .get_mut(CONTINUATION_METADATA)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| protocol("OpenAI continuation metadata is malformed"))?;
            if continuation
                .insert(
                    "response_id".to_owned(),
                    Value::String(response_id.to_owned()),
                )
                .is_some()
            {
                return Err(protocol("OpenAI continuation response ID was already set"));
            }
        }
        Ok(())
    }

    fn commit_part(
        &mut self,
        item_id: &str,
        index: u64,
        completed: &str,
        part: Part,
    ) -> Result<(), LoopError> {
        if let Some(streamed) = self.text.remove(&(item_id.to_owned(), index)) {
            if streamed.text != completed {
                return Err(protocol("completed content differs from streamed deltas"));
            }
            self.queued
                .push_back(ModelTurnEvent::Delta(Delta::CommitPart { part }));
        }
        Ok(())
    }

    fn observe_model(&mut self, value: &Value) -> Result<(), LoopError> {
        let model = value
            .as_str()
            .filter(|value| valid_model(value))
            .ok_or_else(|| protocol("response model is outside canonical bounds"))?;
        if self
            .response_model
            .as_deref()
            .is_some_and(|actual| actual != model)
        {
            return Err(protocol("provider reported inconsistent response models"));
        }
        self.response_model = Some(model.to_owned());
        Ok(())
    }
}

#[async_trait]
impl ModelTurn for OpenAiSubscriptionTurn {
    async fn next_event(
        &mut self,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Ok(Some(event));
            }
            if self.completed {
                return Ok(None);
            }
            let read = tokio::time::timeout(STREAM_IDLE_TIMEOUT, self.stream.next());
            let next = if let Some(cancel) = cancellation.clone() {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(LoopError::Cancelled),
                    value = read => value,
                }
            } else {
                read.await
            }
            .map_err(|_| LoopError::Provider("openai-subscription SSE idle timeout".to_owned()))?;
            let Some(chunk) = next else {
                if !self.stream_ended && !self.buffer.is_empty() {
                    self.stream_ended = true;
                    let frame = std::mem::take(&mut self.buffer);
                    self.consume_frame(&frame)?;
                    continue;
                }
                return Err(protocol("SSE stream closed before response.completed"));
            };
            let chunk = chunk.map_err(|_| {
                LoopError::Provider("openai-subscription stream transport failed".to_owned())
            })?;
            self.total_bytes = self.total_bytes.saturating_add(chunk.len());
            if self.total_bytes > MAX_STREAM_BYTES {
                return Err(protocol("SSE stream exceeds 16 MiB"));
            }
            self.buffer.extend_from_slice(&chunk);
            while let Some((end, delimiter)) = frame_end(&self.buffer) {
                let frame = Zeroizing::new(self.buffer[..end].to_vec());
                self.buffer.drain(..end + delimiter);
                if !frame.is_empty() {
                    self.consume_frame(&frame)?;
                }
            }
            if self.buffer.len() > MAX_EVENT_BYTES {
                return Err(protocol("SSE event exceeds 1 MiB"));
            }
        }
    }
}

fn request_body(
    model: &str,
    request: &TurnRequest,
    binding: &auth::CredentialBinding,
    session_id: &str,
) -> Result<Value, LoopError> {
    if request.transcript.len() > MAX_ITEMS || request.available_tools.len() > MAX_ITEMS {
        return Err(protocol(
            "request contains too many transcript items or tools",
        ));
    }
    if request.structured_output.is_some() {
        return Err(LoopError::Unsupported(
            "openai-subscription structured output is not supported".to_owned(),
        ));
    }
    if request.generation.temperature.is_some() || request.generation.stop_sequences.is_some() {
        return Err(LoopError::Unsupported(
            "openai-subscription sampling controls are not supported".to_owned(),
        ));
    }
    let continuation = ContinuationContext {
        model,
        binding,
        session_id,
    };
    let input = request
        .transcript
        .iter()
        .map(|item| map_item(item, Some(&continuation)))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let tools = request
        .available_tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function", "name": tool.name.0.as_str(), "description": tool.description,
                "parameters": tool.input_schema, "strict": false,
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": model, "input": input, "tools": tools, "tool_choice": "auto",
        "parallel_tool_calls": true, "reasoning": {"summary":"auto"}, "store": false,
        "stream": true, "include": ["reasoning.encrypted_content"],
    });
    // The codex backend rejects max_output_tokens as an unsupported parameter; the
    // requested bound is deliberately not forwarded.
    apply_prompt_cache(&mut body, request)?;
    Ok(body)
}

fn map_item(
    item: &Item,
    continuation: Option<&ContinuationContext<'_>>,
) -> Result<Vec<Value>, LoopError> {
    let role = match item.kind {
        // The codex backend rejects system-role messages; downgrade to developer.
        ItemKind::System | ItemKind::Developer | ItemKind::Context => "developer",
        ItemKind::User => "user",
        ItemKind::Assistant => "assistant",
        ItemKind::Tool => "tool",
        ItemKind::Notification => "user",
    };
    let mut messages = Vec::new();
    let mut content = Vec::new();
    for part in &item.parts {
        match part {
            Part::Text(text) => content.push(json!({
                "type": if role == "assistant" { "output_text" } else { "input_text" },
                "text": if item.kind == ItemKind::Notification { format!("<system-reminder>{}</system-reminder>", text.text) } else if item.kind == ItemKind::Context { format!("Context (not higher-priority instructions):\n{}", text.text) } else { text.text.clone() },
            })),
            Part::Structured(value) => {
                let text = serde_json::to_string(&value.value)
                    .map_err(|_| protocol("structured transcript encoding failed"))?;
                content.push(json!({
                    "type": if role == "assistant" { "output_text" } else { "input_text" },
                    "text": if item.kind == ItemKind::Context { format!("Context (not higher-priority instructions):\n{text}") } else { text },
                }));
            }
            Part::ToolCall(call) => {
                let metadata = continuation
                    .map(|context| continuation_item(&call.metadata, "function_call", context))
                    .transpose()?
                    .flatten();
                let mut value = json!({
                    "type":"function_call", "call_id":call.id.0, "name":call.name,
                    "arguments":serde_json::to_string(&call.input).map_err(|_| protocol("tool-call encoding failed"))?,
                });
                if let Some(metadata) = metadata {
                    value["id"] = Value::String(metadata.item_id.to_owned());
                }
                messages.push(value);
            }
            Part::ToolResult(result) => messages.push(json!({
                "type":"function_call_output", "call_id":result.call_id.0,
                "output":tool_output(&result.output)?,
            })),
            Part::Reasoning(reasoning) => {
                let metadata = continuation
                    .map(|context| continuation_item(&reasoning.metadata, "reasoning", context))
                    .transpose()?
                    .flatten();
                if let Some(metadata) = metadata {
                    messages.push(json!({
                        "id": metadata.item_id,
                        "type": "reasoning",
                        "summary": [],
                        "encrypted_content": metadata.encrypted_content.expect("validated reasoning metadata has encrypted content"),
                    }));
                }
            }
            Part::Media(_) | Part::File(_) | Part::Custom(_) => return Err(LoopError::Unsupported("openai-subscription transcript contains unsupported content".to_owned())),
        }
    }
    if !content.is_empty() && role != "tool" {
        messages.insert(0, json!({"type":"message","role":role,"content":content}));
    }
    Ok(messages)
}

fn tool_output(output: &ToolOutput) -> Result<String, LoopError> {
    match output {
        ToolOutput::Text(value) => Ok(value.clone()),
        ToolOutput::Structured(value) => {
            serde_json::to_string(value).map_err(|_| protocol("tool output encoding failed"))
        }
        ToolOutput::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                Part::Text(text) => Ok(text.text.clone()),
                Part::Structured(value) => serde_json::to_string(&value.value)
                    .map_err(|_| protocol("tool output encoding failed")),
                _ => Err(LoopError::Unsupported(
                    "openai-subscription tool output contains unsupported content".to_owned(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join("\n")),
        ToolOutput::Files(_) => Err(LoopError::Unsupported(
            "openai-subscription file tool output is not supported".to_owned(),
        )),
    }
}

struct ContinuationContext<'a> {
    model: &'a str,
    binding: &'a auth::CredentialBinding,
    session_id: &'a str,
}

struct ContinuationItem<'a> {
    account_digest: &'a str,
    generation: &'a str,
    model: &'a str,
    session_id: &'a str,
    item_id: &'a str,
    encrypted_content: Option<&'a str>,
}

pub(crate) fn durable_reasoning(part: &ReasoningPart) -> bool {
    part.summary.is_none()
        && part.data.is_none()
        && part.redacted
        && validate_continuation_metadata(&part.metadata, "reasoning").is_ok()
}

pub(crate) fn durable_tool_call_metadata(metadata: &MetadataMap) -> bool {
    metadata.contains_key(CONTINUATION_METADATA)
        && validate_continuation_metadata(metadata, "function_call").is_ok()
}

fn continuation_metadata(
    binding: &auth::CredentialBinding,
    model: &str,
    session_id: &str,
    item_id: &str,
    output_index: u64,
    kind: &str,
    encrypted_content: Option<&str>,
) -> MetadataMap {
    let mut continuation = json!({
        "schema_version": CONTINUATION_SCHEMA_VERSION,
        "account_binding": {
            "account_id_digest": hex_digest(&crate::domain::crypto::sha256(binding.account_id.as_bytes())),
            "login_generation": binding.generation,
        },
        "model": model,
        "session_id": session_id,
        "item_id": item_id,
        "output_index": output_index,
        "kind": kind,
    });
    if let Some(encrypted_content) = encrypted_content {
        continuation["encrypted_content"] = Value::String(encrypted_content.to_owned());
    }
    MetadataMap::from([(CONTINUATION_METADATA.to_owned(), continuation)])
}

fn continuation_item<'a>(
    metadata: &'a MetadataMap,
    expected_kind: &str,
    context: &ContinuationContext<'_>,
) -> Result<Option<ContinuationItem<'a>>, LoopError> {
    let Some(_) = metadata.get(CONTINUATION_METADATA) else {
        return Ok(None);
    };
    let item = validate_continuation_metadata(metadata, expected_kind)?;
    let current_digest = hex_digest(&crate::domain::crypto::sha256(
        context.binding.account_id.as_bytes(),
    ));
    if item.account_digest != current_digest
        || item.generation != context.binding.generation
        || item.model != context.model
        || item.session_id != context.session_id
    {
        return Ok(None);
    }
    Ok(Some(item))
}

fn validate_continuation_metadata<'a>(
    metadata: &'a MetadataMap,
    expected_kind: &str,
) -> Result<ContinuationItem<'a>, LoopError> {
    for (name, value) in metadata {
        if name != CONTINUATION_METADATA {
            super::persistence::validate_metadata_entry(name, value).map_err(|_| {
                protocol("OpenAI continuation metadata contains invalid generic metadata")
            })?;
        }
    }
    let value = metadata
        .get(CONTINUATION_METADATA)
        .ok_or_else(|| protocol("OpenAI continuation metadata is missing"))?;
    let object = value
        .as_object()
        .ok_or_else(|| protocol("OpenAI continuation metadata is not an object"))?;
    let expected_fields = if expected_kind == "reasoning" { 9 } else { 8 };
    if object.len() != expected_fields
        || object.get("schema_version").and_then(Value::as_u64) != Some(CONTINUATION_SCHEMA_VERSION)
        || object.get("kind").and_then(Value::as_str) != Some(expected_kind)
    {
        return Err(protocol("OpenAI continuation metadata schema is invalid"));
    }
    let account = object
        .get("account_binding")
        .and_then(Value::as_object)
        .filter(|value| value.len() == 2)
        .ok_or_else(|| protocol("OpenAI continuation account binding is invalid"))?;
    let account_digest = canonical_string(account.get("account_id_digest"), 64)?;
    if account_digest.len() != 64
        || !account_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(protocol("OpenAI continuation account digest is invalid"));
    }
    let generation = canonical_string(account.get("login_generation"), 256)?;
    let model = canonical_string(object.get("model"), 256)?;
    if !valid_model(model) {
        return Err(protocol("OpenAI continuation model is invalid"));
    }
    let session_id = canonical_string(object.get("session_id"), 256)?;
    let _response_id = bounded_id(object.get("response_id"))?;
    let item_id = bounded_id(object.get("item_id"))?;
    object
        .get("output_index")
        .and_then(Value::as_u64)
        .filter(|index| *index < MAX_ITEMS as u64)
        .ok_or_else(|| protocol("OpenAI continuation output index is invalid"))?;
    let encrypted_content = if expected_kind == "reasoning" {
        Some(
            object
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= MAX_TEXT_BYTES)
                .ok_or_else(|| protocol("OpenAI continuation ciphertext is invalid"))?,
        )
    } else {
        None
    };
    Ok(ContinuationItem {
        account_digest,
        generation,
        model,
        session_id,
        item_id,
        encrypted_content,
    })
}

fn canonical_string(value: Option<&Value>, maximum: usize) -> Result<&str, LoopError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= maximum && value.is_ascii())
        .ok_or_else(|| protocol("OpenAI continuation string is invalid"))
}

fn request_idempotency_key(request: &TurnRequest) -> Result<String, LoopError> {
    if let Some(value) = request.metadata.get(PROVIDER_REQUEST_DIGEST) {
        return value
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 512 && value.is_ascii())
            .map(str::to_owned)
            .ok_or_else(|| protocol("durable request digest is invalid"));
    }
    let bytes = serde_json::to_vec(request).map_err(|_| protocol("request digest failed"))?;
    Ok(format!(
        "kit-{}",
        hex_digest(&crate::domain::crypto::sha256(&bytes))
    ))
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        },
    )
}

fn apply_prompt_cache(body: &mut Value, request: &TurnRequest) -> Result<(), LoopError> {
    let Some(cache) = &request.cache else {
        return Ok(());
    };
    if matches!(cache.mode, PromptCacheMode::Disabled) {
        return Ok(());
    }
    if matches!(cache.strategy, PromptCacheStrategy::Explicit { .. })
        && matches!(cache.mode, PromptCacheMode::Required)
    {
        return Err(LoopError::Unsupported(
            "openai-subscription Responses does not support explicit cache breakpoints".to_owned(),
        ));
    }
    if let Some(key) = &cache.key {
        if key.is_empty() || key.len() > 256 || !key.is_ascii() {
            return Err(LoopError::Unsupported(
                "openai-subscription prompt cache key is outside canonical bounds".to_owned(),
            ));
        }
        body["prompt_cache_key"] = Value::String(key.clone());
    }
    // The codex backend rejects prompt_cache_retention as an unsupported parameter and
    // applies its own retention; the requested retention is deliberately not forwarded.
    Ok(())
}

fn classify_response_failure(value: &Value) -> LoopError {
    let error = value.pointer("/response/error").unwrap_or(&Value::Null);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128 && value.is_ascii())
        .unwrap_or("response_failed");
    let error_type = error
        .get("type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128 && value.is_ascii())
        .unwrap_or("unknown");
    let status = error
        .get("status")
        .or_else(|| value.pointer("/response/status_code"))
        .and_then(Value::as_u64);
    let retry_after = error
        .get("retry_after")
        .or_else(|| value.pointer("/response/retry_after"))
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str()?.parse::<u64>().ok())
        })
        .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
    let transient = status.is_some_and(|status| status == 429 || (500..=599).contains(&status))
        || [code, error_type].iter().any(|value| {
            matches!(
                *value,
                "rate_limit_exceeded"
                    | "rate_limit_error"
                    | "rate_limited"
                    | "overloaded"
                    | "server_overloaded"
            )
        });
    if transient {
        return LoopError::TransientProvider {
            message: format!("openai-subscription response failed: {error_type}/{code}"),
            retry_after,
        };
    }
    let authentication = status.is_some_and(|status| status == 401 || status == 403)
        || [code, error_type].iter().any(|value| {
            matches!(
                *value,
                "authentication_error"
                    | "invalid_api_key"
                    | "invalid_authentication"
                    | "unauthorized"
            )
        });
    if authentication {
        LoopError::Provider(
            "openai-subscription authentication failed after inference acceptance".to_owned(),
        )
    } else {
        LoopError::Provider(format!(
            "openai-subscription response failed: {error_type}/{code}"
        ))
    }
}

#[cfg(test)]
fn response_failure(value: &Value) -> LoopError {
    classify_response_failure(value)
}

fn zeroize_encrypted_content(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(encrypted)) = object.get_mut("encrypted_content") {
                encrypted.zeroize();
            }
            for nested in object.values_mut() {
                zeroize_encrypted_content(nested);
            }
        }
        Value::Array(values) => {
            for nested in values {
                zeroize_encrypted_content(nested);
            }
        }
        _ => {}
    }
}

fn bounded_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, LoopError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_EVENT_BYTES)
        .ok_or_else(|| protocol("Responses string field is missing or outside bounds"))
}

fn nonnegative(value: &Value, field: &str) -> Result<u64, LoopError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| protocol("token usage is missing or invalid"))
}

fn frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    [
        (b"\r\n\r\n".as_slice(), 4),
        (b"\n\n".as_slice(), 2),
        (b"\r\r".as_slice(), 2),
    ]
    .into_iter()
    .filter_map(|(delimiter, length)| {
        buffer
            .windows(length)
            .position(|value| value == delimiter)
            .map(|position| (position, length))
    })
    .min_by_key(|(position, _)| *position)
}

fn event_item(
    value: &Value,
    index_field: &str,
    items: &HashMap<String, u64>,
) -> Result<(String, u64), LoopError> {
    let item_id = bounded_id(value.get("item_id"))?;
    let index = nonnegative(value, index_field)?;
    let output_index = nonnegative(value, "output_index")?;
    if items.get(item_id).copied() != Some(output_index) {
        return Err(protocol(
            "content delta refers to an unknown or inconsistent output item",
        ));
    }
    Ok((item_id.to_owned(), index))
}

fn append_part(
    parts: &mut HashMap<(String, u64), PartAccumulator>,
    key: (String, u64),
    delta: &str,
    kind: PartKind,
    queued: &mut VecDeque<ModelTurnEvent>,
) -> Result<(), LoopError> {
    let part = parts.entry(key.clone()).or_insert_with(|| {
        let id = PartId::new(format!(
            "openai-subscription:{}:{}:{}:{}",
            key.0.len(),
            key.0,
            if kind == PartKind::Reasoning {
                "reasoning"
            } else {
                "text"
            },
            key.1
        ));
        queued.push_back(ModelTurnEvent::Delta(Delta::BeginPart {
            part_id: id.clone(),
            kind,
        }));
        PartAccumulator {
            id,
            text: String::new(),
        }
    });
    if part.text.len().saturating_add(delta.len()) > MAX_TEXT_BYTES {
        return Err(protocol("streamed content exceeds 8 MiB"));
    }
    part.text.push_str(delta);
    queued.push_back(ModelTurnEvent::Delta(Delta::AppendText {
        part_id: part.id.clone(),
        chunk: delta.to_owned(),
    }));
    Ok(())
}

fn bounded_id(value: Option<&Value>) -> Result<&str, LoopError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256 && value.is_ascii())
        .ok_or_else(|| protocol("Responses item ID is missing or outside bounds"))
}

fn optional_usage(value: Option<&Value>) -> Result<Option<u64>, LoopError> {
    value
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| protocol("token usage detail is invalid"))
        })
        .transpose()
}

fn valid_model(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:/".contains(&byte))
}

fn validated_model_header(value: &reqwest::header::HeaderValue) -> Result<String, LoopError> {
    value
        .to_str()
        .ok()
        .filter(|value| valid_model(value))
        .map(str::to_owned)
        .ok_or_else(|| protocol("OpenAI-Model response header is outside canonical bounds"))
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds = value.parse::<u64>().ok().or_else(|| {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(std::time::SystemTime::now())
            .ok()
            .map(|duration| duration.as_secs())
    })?;
    Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

/// Bounded, printable excerpt of a failure response body, prefixed for message
/// concatenation; empty when the body is absent or unreadable.
async fn failure_body_excerpt(response: reqwest::Response) -> String {
    const MAX_EXCERPT_BYTES: usize = 1024;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_EXCERPT_BYTES {
            body.truncate(MAX_EXCERPT_BYTES);
            break;
        }
    }
    let excerpt = String::from_utf8_lossy(&body)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let excerpt = excerpt.trim();
    if excerpt.is_empty() {
        String::new()
    } else {
        format!(": {excerpt}")
    }
}

fn known_not_dispatched(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
    )
}

fn model_metadata(header: Option<&str>, observed: Option<&str>) -> MetadataMap {
    let mut metadata = MetadataMap::new();
    if let Some(observed) = observed {
        metadata.insert(
            "openai.observed_response_model".to_owned(),
            Value::String(observed.to_owned()),
        );
        if header.is_some_and(|header| header != observed) {
            metadata.insert("openai.model_header_mismatch".to_owned(), Value::Bool(true));
        }
    }
    metadata
}

fn protocol(message: &str) -> LoopError {
    LoopError::Provider(format!("openai-subscription protocol error: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read as _;

        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 2048];
        loop {
            let count = stream.read(&mut chunk).unwrap();
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    fn response_server(
        responses: Vec<(u16, &'static str, String)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        response_server_with_models(
            responses
                .into_iter()
                .map(|(status, content_type, body)| (status, content_type, None, body))
                .collect(),
        )
    }

    fn response_server_with_models(
        responses: Vec<(u16, &'static str, Option<&'static str>, String)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, content_type, model, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let reason = if status == 200 { "OK" } else { "Unauthorized" };
                let model = model
                    .map(|model| format!("OpenAI-Model: {model}\r\n"))
                    .unwrap_or_default();
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n{model}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        (format!("http://{address}/responses"), handle)
    }

    fn request() -> TurnRequest {
        TurnRequest {
            session_id: "s".into(),
            turn_id: "t".into(),
            transcript: vec![Item::text(ItemKind::User, "hello")],
            available_tools: Vec::new(),
            cache: None,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        }
    }

    fn test_request_body(model: &str, request: &TurnRequest) -> Result<Value, LoopError> {
        request_body(
            model,
            request,
            &auth::CredentialBinding {
                account_id: "test-account".to_owned(),
                generation: "test-generation".to_owned(),
            },
            "s",
        )
    }

    #[test]
    fn request_maps_messages_tools_and_results_in_order() {
        let request = TurnRequest {
            session_id: "s".into(),
            turn_id: "t".into(),
            transcript: vec![
                Item::text(ItemKind::System, "rules"),
                Item::new(
                    ItemKind::Assistant,
                    vec![Part::ToolCall(ToolCallPart::new(
                        "call-1",
                        "read",
                        json!({"path":"x"}),
                    ))],
                ),
                Item::new(
                    ItemKind::Tool,
                    vec![Part::ToolResult(agentkit_core::ToolResultPart::success(
                        "call-1",
                        ToolOutput::text("ok"),
                    ))],
                ),
            ],
            available_tools: vec![agentkit_tools_core::ToolSpec::new(
                "read",
                "Read",
                json!({"type":"object"}),
            )],
            cache: None,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        };
        let body = test_request_body("gpt-5.6-sol", &request).unwrap();
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][1]["call_id"], "call-1");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn generation_and_prompt_cache_controls_map_exactly_or_reject() {
        let mut request = request();
        request.generation.max_output_tokens = Some(37);
        request.cache = Some(
            agentkit_loop::PromptCacheRequest::automatic_required()
                .with_key("explicit-key")
                .with_retention(agentkit_loop::PromptCacheRetention::Extended),
        );
        let body = test_request_body("gpt-5.6-sol", &request).unwrap();
        // The codex backend rejects these parameters, so they must never be forwarded.
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["prompt_cache_key"], "explicit-key");
        assert!(body.get("prompt_cache_retention").is_none());

        request.cache = Some(agentkit_loop::PromptCacheRequest::disabled());
        let body = test_request_body("gpt-5.6-sol", &request).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());

        request.cache = Some(agentkit_loop::PromptCacheRequest::explicit_required([
            agentkit_loop::PromptCacheBreakpoint::tools_end(),
        ]));
        assert!(matches!(
            test_request_body("gpt-5.6-sol", &request),
            Err(LoopError::Unsupported(_))
        ));
        request.cache = None;
        request.generation.temperature = Some(0.5);
        assert!(matches!(
            test_request_body("gpt-5.6-sol", &request),
            Err(LoopError::Unsupported(_))
        ));
    }

    #[test]
    fn encrypted_reasoning_metadata_replays_in_exact_output_order() {
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "gpt-5.6-sol".into(), None);
        for event in [
            json!({"type":"response.created","sequence_number":1,"response":{}}),
            json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"rs_1"}}),
            json!({"type":"response.output_item.added","sequence_number":3,"output_index":1,"item":{"id":"fc_1"}}),
            json!({"type":"response.output_item.done","sequence_number":4,"output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{}","status":"completed","metadata":{"source":"codex"}}}),
            json!({"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque-ciphertext"}}),
            json!({"type":"response.completed","sequence_number":6,"response":{"id":"resp_1"}}),
        ] {
            turn.consume_frame(format!("data: {event}").as_bytes())
                .unwrap();
        }
        let result = turn
            .queued
            .iter()
            .find_map(|event| match event {
                ModelTurnEvent::Finished(result) => Some(result.clone()),
                _ => None,
            })
            .unwrap();
        let Part::Reasoning(reasoning) = &result.output_items[0].parts[0] else {
            panic!("reasoning output order changed")
        };
        assert!(reasoning.redacted);
        assert!(reasoning.summary.is_none());
        assert!(reasoning.data.is_none());
        assert_eq!(
            reasoning.metadata[CONTINUATION_METADATA]["encrypted_content"],
            "opaque-ciphertext"
        );
        let Part::ToolCall(call) = &result.output_items[1].parts[0] else {
            panic!("tool-call output order changed")
        };
        assert_eq!(
            call.metadata[CONTINUATION_METADATA]["response_id"],
            "resp_1"
        );

        let mut result = result;
        let Part::ToolCall(call) = &mut result.output_items[1].parts[0] else {
            unreachable!()
        };
        call.metadata
            .insert("kit.operation_sequence".to_owned(), json!(0));
        call.metadata
            .insert("provider.public".to_owned(), json!({"stable": true}));

        let persisted = result
            .output_items
            .iter()
            .map(crate::agent::agentkit_bridge::mapping::from_agentkit_item)
            .collect::<Vec<_>>();
        let persisted = serde_json::to_vec(&persisted).unwrap();
        assert!(!String::from_utf8_lossy(&persisted).contains("test-account"));
        let persisted: Vec<crate::agent::agentkit_bridge::mapping::CanonicalItem> =
            serde_json::from_slice(&persisted).unwrap();
        let restored = persisted
            .iter()
            .map(crate::agent::agentkit_bridge::mapping::to_agentkit_item)
            .collect::<Vec<_>>();
        assert_eq!(restored, result.output_items);

        let mut followup = request();
        followup.transcript = restored;
        followup.transcript.push(Item::new(
            ItemKind::Tool,
            vec![Part::ToolResult(agentkit_core::ToolResultPart::success(
                "call_1",
                ToolOutput::text("ok"),
            ))],
        ));
        let body = test_request_body("gpt-5.6-sol", &followup).unwrap();
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["id"], "rs_1");
        assert_eq!(body["input"][0]["encrypted_content"], "opaque-ciphertext");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");

        let Part::ToolCall(call) = &followup.transcript[1].parts[0] else {
            unreachable!()
        };
        let mut invalid = call.metadata.clone();
        invalid.insert("api_key".to_owned(), json!("unredacted"));
        assert!(validate_continuation_metadata(&invalid, "function_call").is_err());
    }

    #[test]
    fn continuation_metadata_is_ignored_after_account_model_or_session_switch() {
        let metadata = continuation_metadata(
            &auth::CredentialBinding {
                account_id: "account-a".to_owned(),
                generation: "generation-a".to_owned(),
            },
            "gpt-5.6-sol",
            "session-a",
            "rs_1",
            0,
            "reasoning",
            Some("ciphertext"),
        );
        let mut metadata = metadata;
        metadata.get_mut(CONTINUATION_METADATA).unwrap()["response_id"] = json!("resp_1");
        let item = Item::new(
            ItemKind::Assistant,
            vec![Part::Reasoning(ReasoningPart {
                summary: None,
                data: None,
                redacted: true,
                metadata: metadata.clone(),
            })],
        );
        let mut tool_metadata = continuation_metadata(
            &auth::CredentialBinding {
                account_id: "account-a".to_owned(),
                generation: "generation-a".to_owned(),
            },
            "gpt-5.6-sol",
            "session-a",
            "fc_1",
            1,
            "function_call",
            None,
        );
        tool_metadata.get_mut(CONTINUATION_METADATA).unwrap()["response_id"] = json!("resp_1");
        let tool = Item::new(
            ItemKind::Assistant,
            vec![Part::ToolCall(
                ToolCallPart::new("call_1", "read", json!({})).with_metadata(tool_metadata.clone()),
            )],
        );
        for (account, generation, model, session) in [
            ("account-b", "generation-a", "gpt-5.6-sol", "session-a"),
            ("account-a", "generation-b", "gpt-5.6-sol", "session-a"),
            ("account-a", "generation-a", "gpt-5.5", "session-a"),
            ("account-a", "generation-a", "gpt-5.6-sol", "session-b"),
        ] {
            let mapped = map_item(
                &item,
                Some(&ContinuationContext {
                    model,
                    binding: &auth::CredentialBinding {
                        account_id: account.to_owned(),
                        generation: generation.to_owned(),
                    },
                    session_id: session,
                }),
            )
            .unwrap();
            assert!(mapped.is_empty());
            let mapped_tool = map_item(
                &tool,
                Some(&ContinuationContext {
                    model,
                    binding: &auth::CredentialBinding {
                        account_id: account.to_owned(),
                        generation: generation.to_owned(),
                    },
                    session_id: session,
                }),
            )
            .unwrap();
            assert_eq!(mapped_tool[0]["type"], "function_call");
            assert!(mapped_tool[0].get("id").is_none());
            assert_eq!(
                item.parts[0],
                Part::Reasoning(ReasoningPart {
                    summary: None,
                    data: None,
                    redacted: true,
                    metadata: metadata.clone(),
                })
            );
            assert_eq!(
                tool.parts[0],
                Part::ToolCall(
                    ToolCallPart::new("call_1", "read", json!({}))
                        .with_metadata(tool_metadata.clone())
                )
            );
        }
        let generic = map_item(&tool, None).unwrap();
        assert_eq!(generic[0]["type"], "function_call");
        assert!(generic[0].get("id").is_none());
        assert!(!durable_reasoning(&ReasoningPart::summary(
            "private plaintext"
        )));
    }

    #[test]
    fn cache_usage_categories_are_exclusive_and_underflow_safe() {
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
        for event in [
            json!({"type":"response.created","sequence_number":1,"response":{}}),
            json!({"type":"response.completed","sequence_number":2,"response":{"id":"resp","usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":1}}}}),
        ] {
            turn.consume_frame(format!("data: {event}").as_bytes())
                .unwrap();
        }
        let tokens = turn.usage.unwrap().tokens.unwrap();
        assert_eq!(tokens.input_tokens, 5);
        assert_eq!(tokens.cached_input_tokens, Some(3));
        assert_eq!(tokens.cache_write_input_tokens, Some(2));
        assert_eq!(tokens.output_tokens, 3);
        assert_eq!(tokens.reasoning_tokens, Some(1));

        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut invalid = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
        invalid
            .consume_frame(br#"data: {"type":"response.created","response":{}}"#)
            .unwrap();
        assert!(
            invalid
                .consume_frame(br#"data: {"type":"response.completed","response":{"id":"resp","usage":{"input_tokens":4,"output_tokens":1,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2}}}}"#)
                .is_err()
        );
    }

    #[test]
    fn incomplete_returns_partial_output_and_typed_finish_reasons() {
        for (reason, expected) in [
            ("max_output_tokens", FinishReason::MaxTokens),
            ("content_filter", FinishReason::Blocked),
        ] {
            let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
            let mut turn = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
            for event in [
                json!({"type":"response.created","sequence_number":1,"response":{}}),
                json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_1"}}),
                json!({"type":"response.output_text.delta","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"partial"}),
                json!({"type":"response.incomplete","sequence_number":4,"response":{"id":"resp","incomplete_details":{"reason":reason}}}),
            ] {
                turn.consume_frame(format!("data: {event}").as_bytes())
                    .unwrap();
            }
            let ModelTurnEvent::Finished(result) = turn.queued.back().unwrap() else {
                panic!("missing finished event")
            };
            assert_eq!(result.finish_reason, expected);
            assert!(matches!(
                &result.output_items[0].parts[0],
                Part::Text(text) if text.text == "partial"
            ));
        }
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
        turn.consume_frame(br#"data: {"type":"response.created","response":{}}"#)
            .unwrap();
        assert!(
            turn.consume_frame(br#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"provider_other"}}}"#)
                .is_err()
        );
    }

    #[test]
    fn incomplete_and_failed_responses_never_authorize_buffered_tool_calls() {
        for terminal in [
            json!({"type":"response.incomplete","sequence_number":4,"response":{"id":"resp","incomplete_details":{"reason":"content_filter"}}}),
            json!({"type":"response.failed","sequence_number":4,"response":{"error":{"type":"invalid_request_error","status":400}}}),
        ] {
            let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
            let mut turn = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
            for event in [
                json!({"type":"response.created","sequence_number":1,"response":{}}),
                json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"fc_1"}}),
                json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}}),
            ] {
                turn.consume_frame(format!("data: {event}").as_bytes())
                    .unwrap();
            }
            assert!(
                turn.queued
                    .iter()
                    .all(|event| !matches!(event, ModelTurnEvent::ToolCall(_)))
            );
            let failed = terminal["type"] == "response.failed";
            let result = turn.consume_frame(format!("data: {terminal}").as_bytes());
            assert_eq!(result.is_err(), failed);
            assert!(
                turn.queued
                    .iter()
                    .all(|event| !matches!(event, ModelTurnEvent::ToolCall(_)))
            );
            if !failed {
                let ModelTurnEvent::Finished(result) = turn.queued.back().unwrap() else {
                    panic!("missing incomplete result")
                };
                assert_eq!(result.finish_reason, FinishReason::Blocked);
                assert!(result.output_items.is_empty());
            }
        }
    }

    #[test]
    fn response_failed_classifies_transient_auth_and_fatal_errors() {
        let transient = response_failure(
            &json!({"response":{"error":{"type":"overloaded","code":"server_overloaded","status":503,"retry_after":"7"}}}),
        );
        assert!(matches!(
            transient,
            LoopError::TransientProvider {
                retry_after: Some(value),
                ..
            } if value == Duration::from_secs(7)
        ));
        let auth = response_failure(
            &json!({"response":{"error":{"type":"authentication_error","status":401}}}),
        );
        assert!(!auth.is_retryable());
        assert!(auth.to_string().contains("after inference acceptance"));
        let fatal = response_failure(
            &json!({"response":{"error":{"type":"invalid_request_error","code":"bad_input","status":400}}}),
        );
        assert!(!fatal.is_retryable());
        assert!(
            fatal
                .to_string()
                .contains("invalid_request_error/bad_input")
        );
    }

    #[test]
    fn malformed_order_and_duplicate_sequence_fail_closed() {
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "gpt-5.6-sol".into(), None);
        assert!(
            turn.consume_frame(
                br#"data: {"type":"response.output_text.delta","sequence_number":1,"delta":"x"}"#
            )
            .is_err()
        );
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "gpt-5.6-sol".into(), None);
        turn.consume_frame(
            br#"data: {"type":"response.created","sequence_number":1,"response":{}}"#,
        )
        .unwrap();
        assert!(
            turn.consume_frame(br#"data: {"type":"response.metadata","sequence_number":1}"#)
                .is_err()
        );
    }

    #[test]
    fn duplicate_function_call_item_and_output_ids_fail_before_tool_emission() {
        let cases = [
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1"}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}}),
                json!({"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1"}}),
            ],
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1"}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_2"}}),
            ],
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1"}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}}),
                json!({"type":"response.output_item.added","output_index":1,"item":{"id":"fc_2"}}),
                json!({"type":"response.output_item.done","output_index":1,"item":{"id":"fc_2","type":"function_call","call_id":"call_1","name":"write","arguments":"{}"}}),
            ],
        ];
        for events in cases {
            let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
            let mut turn = OpenAiSubscriptionTurn::new(stream, "gpt-5.6-sol".into(), None);
            turn.consume_frame(br#"data: {"type":"response.created","response":{}}"#)
                .unwrap();
            let mut result = Ok(());
            for event in events {
                result = turn.consume_frame(format!("data: {event}").as_bytes());
                if result.is_err() {
                    break;
                }
            }
            assert!(result.is_err());
            assert!(
                turn.queued
                    .iter()
                    .all(|event| !matches!(event, ModelTurnEvent::ToolCall(_)))
            );
        }
    }

    #[test]
    fn text_function_call_usage_and_completion_map_to_agentkit_events() {
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "gpt-5.6-sol".into(), None);
        for event in [
            json!({"type":"response.created","sequence_number":1,"response":{}}),
            json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_1"}}),
            json!({"type":"response.output_text.delta","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"hello"}),
            json!({"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}),
            json!({"type":"response.output_item.added","sequence_number":5,"output_index":1,"item":{"id":"fc_1"}}),
            json!({"type":"response.output_item.done","sequence_number":6,"output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"x\"}"}}),
            json!({"type":"response.completed","sequence_number":7,"response":{"id":"resp_1","model":"gpt-5.6-sol-actual","usage":{"input_tokens":5,"output_tokens":2,"input_tokens_details":{"cached_tokens":1},"output_tokens_details":{"reasoning_tokens":1}}}}),
        ] {
            turn.consume_frame(format!("data: {event}").as_bytes())
                .unwrap();
        }
        assert!(turn.tool_call);
        assert_eq!(turn.response_id.as_deref(), Some("resp_1"));
        assert_eq!(
            turn.usage
                .as_ref()
                .unwrap()
                .tokens
                .as_ref()
                .unwrap()
                .input_tokens,
            4
        );
        assert!(
            turn.queued.iter().any(
                |event| matches!(event, ModelTurnEvent::ToolCall(call) if call.id.0 == "call_1")
            )
        );
        assert!(
            matches!(turn.queued.back(), Some(ModelTurnEvent::Finished(result)) if result.finish_reason == FinishReason::ToolCall)
        );
        assert!(
            matches!(turn.queued.back(), Some(ModelTurnEvent::Finished(result)) if result.model.as_deref() == Some("gpt-5.6-sol-actual"))
        );
    }

    #[test]
    fn multiple_messages_have_unique_stable_parts_and_reasoning_stays_opaque() {
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
        for event in [
            json!({"type":"response.created","sequence_number":1,"response":{}}),
            json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"reason_1"}}),
            json!({"type":"response.reasoning_summary_text.delta","sequence_number":3,"item_id":"reason_1","output_index":0,"summary_index":0,"delta":"first"}),
            json!({"type":"response.reasoning_summary_text.delta","sequence_number":4,"item_id":"reason_1","output_index":0,"summary_index":1,"delta":"second"}),
            json!({"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"reason_1","type":"reasoning","summary":[{"text":"first"},{"text":"second"}],"encrypted_content":"opaque"}}),
            json!({"type":"response.output_item.added","sequence_number":6,"output_index":1,"item":{"id":"msg_1"}}),
            json!({"type":"response.output_text.delta","sequence_number":7,"item_id":"msg_1","output_index":1,"content_index":0,"delta":"one"}),
            json!({"type":"response.output_item.done","sequence_number":8,"output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"one"}]}}),
            json!({"type":"response.output_item.added","sequence_number":9,"output_index":2,"item":{"id":"msg_2"}}),
            json!({"type":"response.output_text.delta","sequence_number":10,"item_id":"msg_2","output_index":2,"content_index":0,"delta":"two"}),
            json!({"type":"response.output_item.done","sequence_number":11,"output_index":2,"item":{"id":"msg_2","type":"message","role":"assistant","content":[{"type":"output_text","text":"two"}]}}),
            json!({"type":"response.completed","sequence_number":12,"response":{"id":"resp","usage":{"input_tokens":2,"output_tokens":3}}}),
        ] {
            turn.consume_frame(format!("data: {event}").as_bytes())
                .unwrap();
        }
        let ids = turn
            .queued
            .iter()
            .filter_map(|event| match event {
                ModelTurnEvent::Delta(Delta::BeginPart { part_id, .. }) => Some(part_id.clone()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), 2);
        assert!(turn.text.is_empty());
        let ModelTurnEvent::Finished(result) = turn.queued.back().unwrap() else {
            panic!("missing result")
        };
        assert!(matches!(
            result.output_items[0].parts[0],
            Part::Reasoning(_)
        ));
    }

    #[test]
    fn sse_comments_crlf_and_multiline_data_are_supported_and_usage_underflow_fails() {
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
        turn.consume_frame(b": heartbeat\r\n").unwrap();
        turn.consume_frame(
            b"event: response.created\r\ndata: {\"type\":\"response.created\",\r\ndata: \"sequence_number\":1,\"response\":{}}\r\n: heartbeat",
        )
        .unwrap();
        assert!(turn.created);
        let error = turn
            .consume_frame(
                br#"data: {"type":"response.completed","sequence_number":2,"response":{"id":"resp","usage":{"input_tokens":1,"output_tokens":1,"input_tokens_details":{"cached_tokens":2}}}}"#,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("nested input token categories exceed")
        );
    }

    #[tokio::test]
    async fn eof_flushes_a_final_completed_event_without_blank_line() {
        let bytes = Bytes::from(
            "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{}}\n\n\
             data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp\",\"model\":\"actual-model\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}",
        );
        let stream = futures_util::stream::iter([Ok(bytes)]);
        let mut turn = OpenAiSubscriptionTurn::new(stream, "requested".into(), None);
        let mut finished = None;
        while let Some(event) = turn.next_event(None).await.unwrap() {
            if let ModelTurnEvent::Finished(result) = event {
                finished = Some(result);
            }
        }
        assert_eq!(finished.unwrap().model.as_deref(), Some("actual-model"));
    }

    #[test]
    fn transcript_roles_content_types_and_private_reasoning_are_canonical() {
        let context = map_item(&Item::text(ItemKind::Context, "facts"), None).unwrap();
        assert_eq!(context[0]["role"], "developer");
        assert!(
            context[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Context (not higher-priority instructions):")
        );
        let assistant = map_item(
            &Item::new(
                ItemKind::Assistant,
                vec![
                    Part::Text(TextPart::new("text")),
                    Part::Structured(agentkit_core::StructuredPart::new(json!({"ok":true}))),
                    Part::Reasoning(ReasoningPart::summary("do not replay")),
                ],
            ),
            None,
        )
        .unwrap();
        assert!(
            assistant[0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|part| part["type"] == "output_text")
        );
        assert!(!assistant[0].to_string().contains("do not replay"));
    }

    #[test]
    fn retry_after_is_bounded_and_transient_errors_are_typed() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "99999".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(MAX_RETRY_AFTER));
        let error = LoopError::TransientProvider {
            message: "rate limited".into(),
            retry_after: retry_after(&headers),
        };
        assert!(error.is_retryable());
        assert_eq!(error.retry_after(), Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn header_is_authoritative_and_observed_model_is_retained() {
        let stream = futures_util::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut turn =
            OpenAiSubscriptionTurn::new(stream, "requested".into(), Some("header-model".into()));
        turn.consume_frame(
            br#"data: {"type":"response.created","sequence_number":1,"response":{"model":"different-model"}}"#,
        )
        .unwrap();
        turn.consume_frame(
            br#"data: {"type":"response.completed","sequence_number":2,"response":{"id":"response","model":"different-model"}}"#,
        )
        .unwrap();
        let ModelTurnEvent::Finished(result) = turn.queued.back().unwrap() else {
            panic!("missing result")
        };
        assert_eq!(result.model.as_deref(), Some("header-model"));
        assert_eq!(
            result.metadata.get("openai.observed_response_model"),
            Some(&json!("different-model"))
        );
        assert_eq!(
            result.metadata.get("openai.model_header_mismatch"),
            Some(&json!(true))
        );
        assert!(validated_model_header(&"bad model".parse().unwrap()).is_err());
    }

    #[tokio::test]
    async fn http_model_header_is_captured_before_sse_and_reported_authoritatively() {
        let stream = [
            json!({"type":"response.created","sequence_number":1,"response":{"model":"header-model"}}),
            json!({"type":"response.completed","sequence_number":2,"response":{"id":"resp","model":"header-model"}}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let (endpoint, server) = response_server_with_models(vec![(
            200,
            "text/event-stream",
            Some("header-model"),
            stream,
        )]);
        let config = SubscriptionConfig::new("gpt-5.6-sol".into())
            .unwrap()
            .with_test_endpoint(endpoint)
            .with_test_credentials(vec![auth::TokenRecord::for_test("token", "account")]);
        let adapter = OpenAiSubscriptionAdapter::new(config).unwrap();
        let mut session = adapter
            .start_session(SessionConfig::new("session"))
            .await
            .unwrap();
        let mut turn = session.begin_turn(request(), None).await.unwrap();
        let mut result = None;
        while let Some(event) = turn.next_event(None).await.unwrap() {
            if let ModelTurnEvent::Finished(value) = event {
                result = Some(value);
            }
        }
        assert_eq!(result.unwrap().model.as_deref(), Some("header-model"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn http_429_retries_once_with_the_same_idempotency_key() {
        let completed = [
            json!({"type":"response.created","sequence_number":1,"response":{}}),
            json!({"type":"response.completed","sequence_number":2,"response":{"id":"accepted-once"}}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let (endpoint, server) = response_server(vec![
            (
                429,
                "application/json",
                "{\"error\":\"rate_limited\"}".into(),
            ),
            (200, "text/event-stream", completed),
        ]);
        let config = SubscriptionConfig::new("gpt-5.6-sol".into())
            .unwrap()
            .with_test_endpoint(endpoint)
            .with_test_credentials(vec![auth::TokenRecord::for_test("token", "account")]);
        let adapter = OpenAiSubscriptionAdapter::new(config).unwrap();
        let mut session = adapter
            .start_session(SessionConfig::new("session"))
            .await
            .unwrap();
        let mut turn = session.begin_turn(request(), None).await.unwrap();
        let mut accepted = 0;
        while let Some(event) = turn.next_event(None).await.unwrap() {
            if matches!(event, ModelTurnEvent::Finished(_)) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 1);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        let keys = requests
            .iter()
            .map(|request| {
                request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("idempotency-key:"))
                    .unwrap()
                    .to_owned()
            })
            .collect::<HashSet<_>>();
        assert_eq!(keys.len(), 1);
    }

    #[tokio::test]
    async fn exhausted_known_not_dispatched_retry_is_terminal_without_outer_retry() {
        for status in [429, 503] {
            let (endpoint, server) = response_server(vec![
                (status, "application/json", "{}".into()),
                (status, "application/json", "{}".into()),
            ]);
            let config = SubscriptionConfig::new("gpt-5.6-sol".into())
                .unwrap()
                .with_test_endpoint(endpoint)
                .with_test_credentials(vec![auth::TokenRecord::for_test("token", "account")]);
            let adapter = OpenAiSubscriptionAdapter::new(config).unwrap();
            let mut session = adapter
                .start_session(SessionConfig::new("session"))
                .await
                .unwrap();
            let error = match session.begin_turn(request(), None).await {
                Err(error) => error,
                Ok(_) => panic!("second known-not-dispatched response was accepted"),
            };
            assert!(matches!(error, LoopError::Provider(_)));
            assert!(!error.is_retryable());
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[test]
    fn endpoint_override_exists_only_in_test_configuration() {
        let config = SubscriptionConfig::new("gpt-5.6-sol".into())
            .unwrap()
            .with_test_endpoint("http://127.0.0.1:1/responses".into());
        assert_eq!(config.endpoint(), "http://127.0.0.1:1/responses");
    }

    #[tokio::test]
    async fn loopback_response_refreshes_one_401_and_streams_completion() {
        let stream = [
            json!({"type":"response.created","sequence_number":1,"response":{}}),
            json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_1"}}),
            json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}),
            json!({"type":"response.completed","sequence_number":4,"response":{"id":"resp_1","usage":{"input_tokens":1,"output_tokens":1}}}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let (endpoint, server) = response_server(vec![
            (401, "application/json", String::new()),
            (200, "text/event-stream", stream),
        ]);
        let config = SubscriptionConfig::new("gpt-5.6-sol".into())
            .unwrap()
            .with_test_endpoint(endpoint)
            .with_test_credentials(vec![
                auth::TokenRecord::for_test("old-token", "account-1"),
                auth::TokenRecord::for_test("new-token", "account-1"),
            ]);
        let adapter = OpenAiSubscriptionAdapter::new(config).unwrap();
        let mut session = adapter
            .start_session(SessionConfig::new("session-1"))
            .await
            .unwrap();
        let mut turn = session.begin_turn(request(), None).await.unwrap();
        let mut result = None;
        while let Some(event) = turn.next_event(None).await.unwrap() {
            if let ModelTurnEvent::Finished(value) = event {
                result = Some(value);
            }
        }
        let result = result.unwrap();
        assert_eq!(result.response_id.as_deref(), Some("resp_1"));
        assert!(result.model.is_none());
        assert!(matches!(
            &result.output_items[0].parts[0],
            Part::Text(text) if text.text == "ok"
        ));

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("authorization: Bearer old-token"));
        assert!(requests[1].contains("authorization: Bearer new-token"));
        assert!(
            requests
                .iter()
                .all(|request| request.contains("chatgpt-account-id: account-1"))
        );
        assert!(
            requests
                .iter()
                .all(|request| request.contains("\"store\":false"))
        );
    }

    #[tokio::test]
    async fn sse_auth_failure_is_not_replayed_after_dispatch() {
        let failed = [
            json!({"type":"response.created","sequence_number":1,"response":{}}),
            json!({"type":"response.failed","sequence_number":2,"response":{"error":{"type":"authentication_error","status":401}}}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let (endpoint, server) = response_server(vec![(200, "text/event-stream", failed)]);
        let config = SubscriptionConfig::new("gpt-5.6-sol".into())
            .unwrap()
            .with_test_endpoint(endpoint)
            .with_test_credentials(vec![
                auth::TokenRecord::for_test("old-token", "account-1"),
                auth::TokenRecord::for_test("new-token", "account-1"),
            ]);
        let adapter = OpenAiSubscriptionAdapter::new(config).unwrap();
        let mut session = adapter
            .start_session(SessionConfig::new("session-1"))
            .await
            .unwrap();
        let mut turn = session.begin_turn(request(), None).await.unwrap();
        let error = loop {
            match turn.next_event(None).await {
                Ok(Some(_)) => continue,
                Err(error) => break error,
                Ok(None) => panic!("failed response ended without an error"),
            }
        };
        assert!(error.to_string().contains("after inference acceptance"));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("authorization: Bearer old-token"));
    }

    #[tokio::test]
    async fn credential_account_or_generation_switch_stales_session_before_wire() {
        for switched in [
            auth::TokenRecord::for_test_generation("new", "account-2", "generation-1"),
            auth::TokenRecord::for_test_generation("new", "account-1", "generation-2"),
        ] {
            let config = SubscriptionConfig::new("gpt-5.6-sol".into())
                .unwrap()
                .with_test_endpoint("http://127.0.0.1:1/responses".into())
                .with_test_credentials(vec![auth::TokenRecord::for_test_generation(
                    "old",
                    "account-1",
                    "generation-1",
                )]);
            let credentials = config.credentials.as_ref().unwrap().clone();
            let adapter = OpenAiSubscriptionAdapter::new(config).unwrap();
            let mut session = adapter
                .start_session(SessionConfig::new("session"))
                .await
                .unwrap();
            *credentials.lock().unwrap() = VecDeque::from([switched]);
            assert!(matches!(
                session.begin_turn(request(), None).await,
                Err(LoopError::SessionStale(_))
            ));
        }
    }

    #[tokio::test]
    async fn pending_stream_observes_cancellation() {
        let stream = futures_util::stream::pending::<Result<Bytes, reqwest::Error>>();
        let mut turn = OpenAiSubscriptionTurn::new(stream, "gpt-5.6-sol".into(), None);
        let controller = agentkit_core::CancellationController::new();
        let cancellation = agentkit_core::TurnCancellation::new(controller.handle());
        controller.interrupt();
        assert!(matches!(
            turn.next_event(Some(cancellation)).await,
            Err(LoopError::Cancelled)
        ));
    }

    #[tokio::test]
    #[ignore = "requires kit auth login openai and live subscription entitlement"]
    async fn real_openai_subscription_provider_smoke() {
        let adapter =
            OpenAiSubscriptionAdapter::new(SubscriptionConfig::new("gpt-5.6-sol".into()).unwrap())
                .unwrap();
        let mut session = adapter
            .start_session(SessionConfig::new("kit-real-subscription-smoke"))
            .await
            .unwrap();
        let request = TurnRequest {
            session_id: "kit-real-subscription-smoke".into(),
            turn_id: "turn-1".into(),
            transcript: vec![Item::text(ItemKind::User, "Reply with OK.")],
            available_tools: Vec::new(),
            cache: None,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        };
        let mut turn = session.begin_turn(request, None).await.unwrap();
        loop {
            match turn.next_event(None).await.unwrap() {
                Some(ModelTurnEvent::Finished(result)) => {
                    assert_eq!(result.finish_reason, FinishReason::Completed);
                    assert!(!result.output_items.is_empty());
                    break;
                }
                Some(_) => {}
                None => panic!("provider stream ended without Finished"),
            }
        }
    }
}
