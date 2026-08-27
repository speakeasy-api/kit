use std::{
    collections::{HashMap, HashSet, VecDeque},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use agentkit_core::{
    DataRef, Delta, FinishReason, Item, ItemKind, MediaPart, MetadataMap, Modality, Part, PartId,
    PartKind, ReasoningPart, TextPart, TokenUsage, ToolCallPart, ToolOutput, Usage,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    PromptCacheMode, PromptCacheStrategy, SessionConfig, TurnRequest,
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use serde_json::{Value, json};
use zeroize::{Zeroize, Zeroizing};

use super::credentials as auth;

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODELS_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const X_CODEX_TURN_STATE: &str = "x-codex-turn-state";
// The catalog filters out models newer than this protocol client version. Keep
// it aligned with the newest model schema Kit supports, not Kit's own version.
const MODEL_CATALOG_CLIENT_VERSION: &str = "0.144.0";
const MAX_MODELS_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 1_000;
// TUI attachments allow 20 MiB raw; base64 and JSON add roughly one third.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_WIRE_BYTES: usize = 4 * MAX_STREAM_BYTES;
// Image-generation results arrive as one base64 field in one SSE event.
const MAX_EVENT_BYTES: usize = MAX_STREAM_BYTES;
const MAX_FIELD_BYTES: usize = 1024 * 1024;
const MAX_ITEMS: usize = 10_000;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const RETRY_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);
const RATE_LIMIT_MAX_WAIT: Duration = Duration::from_secs(10 * 60);
const RATE_LIMIT_RESET_GRACE: Duration = Duration::from_secs(10 * 60);
const MAX_RETRY_HINT: Duration = RATE_LIMIT_MAX_WAIT;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PROVIDER_REQUEST_DIGEST: &str = "kit.model_call.request_digest";
const PROVIDER_FINISH_REASONS_METADATA: &str = "agentkit.provider_finish_reasons";
const GENERATED_IMAGE_METADATA: &str = "openai.subscription.generated_image.v1";
pub(crate) const CONTINUATION_METADATA: &str = "openai.subscription.v1";
const CONTINUATION_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Debug)]
pub struct SubscriptionConfig {
    pub model: String,
    pub credential_storage: crate::credentials::CredentialStorage,
    #[cfg(test)]
    endpoint: Option<String>,
}

impl SubscriptionConfig {
    pub fn new(model: String) -> Result<Self, String> {
        if !supported_model(&model) {
            return Err("openai-subscription model is not in the supported model set".to_owned());
        }
        Ok(Self {
            model,
            credential_storage: Default::default(),
            #[cfg(test)]
            endpoint: None,
        })
    }

    pub(crate) fn with_credential_storage(
        mut self,
        credential_storage: crate::credentials::CredentialStorage,
    ) -> Self {
        self.credential_storage = credential_storage;
        self
    }

    fn endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            return endpoint;
        }
        ENDPOINT
    }

    #[cfg(test)]
    fn with_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = Some(endpoint);
        self
    }
}

pub fn supported_model(model: &str) -> bool {
    matches!(
        model,
        "gpt-5.6-sol" | "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.3-codex-spark"
    )
}

#[derive(Clone)]
pub struct OpenAiSubscriptionAdapter {
    config: SubscriptionConfig,
    reasoning_effort: Option<super::adapter::ReasoningEffort>,
    client: reqwest::Client,
    context_windows: Arc<tokio::sync::OnceCell<Arc<HashMap<String, u64>>>>,
}

impl OpenAiSubscriptionAdapter {
    pub fn new(config: SubscriptionConfig) -> Result<Self, String> {
        Self::new_with_reasoning_effort(config, None)
    }

    pub(crate) fn new_with_reasoning_effort(
        config: SubscriptionConfig,
        reasoning_effort: Option<super::adapter::ReasoningEffort>,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "could not build openai-subscription TLS client".to_owned())?;
        Ok(Self {
            config,
            reasoning_effort,
            client,
            context_windows: Arc::new(tokio::sync::OnceCell::new()),
        })
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
        // Model discovery is best-effort: inference should remain available if
        // the catalog endpoint is temporarily unavailable. Without a reported
        // window AgentKit simply omits the ACP context gauge.
        let context_windows = self
            .context_windows
            .get_or_try_init(|| async {
                fetch_context_windows(&self.client, &credentials)
                    .await
                    .map(Arc::new)
            })
            .await
            .cloned()
            .unwrap_or_default();
        Ok(OpenAiSubscriptionSession {
            config: self.config.clone(),
            reasoning_effort: self.reasoning_effort,
            response_attempt_replacement: crate::response_attempt::enabled(&config),
            client: self.client.clone(),
            session_id,
            binding,
            context_windows,
            #[cfg(test)]
            test_credentials: None,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("openai-subscription")
    }
}

pub struct OpenAiSubscriptionSession {
    config: SubscriptionConfig,
    reasoning_effort: Option<super::adapter::ReasoningEffort>,
    response_attempt_replacement: bool,
    client: reqwest::Client,
    session_id: String,
    binding: auth::CredentialBinding,
    context_windows: Arc<HashMap<String, u64>>,
    #[cfg(test)]
    test_credentials: Option<auth::TokenRecord>,
}

struct OpenAiSubscriptionRequestContext {
    config: SubscriptionConfig,
    client: reqwest::Client,
    binding: auth::CredentialBinding,
    session_id: String,
    body_bytes: Zeroizing<Vec<u8>>,
    idempotency_key: String,
    started: tokio::time::Instant,
    deadline: tokio::time::Instant,
    retries: usize,
    credentials: auth::TokenRecord,
    unauthorized: bool,
    turn_state: Option<reqwest::header::HeaderValue>,
    turn_state_from_header: bool,
    wire_bytes: usize,
    #[cfg(test)]
    test_credentials: Option<auth::TokenRecord>,
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
        let started = tokio::time::Instant::now();
        let credentials = self
            .credentials_with_budget(None, cancellation.clone(), started, 1)
            .await?;
        self.ensure_binding(&credentials)?;
        let idempotency_key = request_idempotency_key(&request)?;
        let mut body = request_body(
            &self.config.model,
            self.reasoning_effort,
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
        let mut context = OpenAiSubscriptionRequestContext {
            config: self.config.clone(),
            client: self.client.clone(),
            binding: self.binding.clone(),
            session_id: self.session_id.clone(),
            body_bytes,
            idempotency_key,
            started,
            deadline: started + RETRY_BUDGET,
            retries: 0,
            credentials,
            unauthorized: false,
            turn_state: None,
            turn_state_from_header: false,
            wire_bytes: 0,
            #[cfg(test)]
            test_credentials: self.test_credentials.clone(),
        };
        loop {
            let response = context.send_response(cancellation.clone()).await?;
            let response_model = response
                .headers()
                .get("openai-model")
                .map(validated_model_header)
                .transpose()?;
            let response_request_id = crate::fatal::safe_response_request_id(response.headers());
            let wire_bytes = context.wire_bytes;
            let attempt_deadline = context
                .deadline
                .min(tokio::time::Instant::now() + ATTEMPT_TIMEOUT);
            let mut turn = OpenAiSubscriptionTurn::new_inner(
                response.bytes_stream(),
                OpenAiSubscriptionTurnInit {
                    requested_model: self.config.model.clone(),
                    header_model: response_model,
                    turn_state: context
                        .turn_state
                        .as_ref()
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                    turn_state_from_header: context.turn_state_from_header,
                    binding: self.binding.clone(),
                    session_id: self.session_id.clone(),
                    context_windows: self.context_windows.clone(),
                    response_attempt_replacement: self.response_attempt_replacement,
                    attempt: context.retries.saturating_add(1),
                    response_request_id,
                    wire_bytes,
                    attempt_wire_bytes: 0,
                    request_context: Some(context),
                },
            );
            let first_event = tokio::time::timeout_at(
                attempt_deadline,
                turn.next_event_inner(cancellation.clone()),
            )
            .await;
            let (error, retry_after) = match first_event {
                Ok(Ok(Some(event))) => {
                    turn.queued.push_front(event);
                    return Ok(turn);
                }
                Ok(Ok(None)) => return Ok(turn),
                Ok(Err(error)) => {
                    let retry_after = turn
                        .pending_failure
                        .as_ref()
                        .filter(|failure| failure.retriable)
                        .map(|failure| failure.retry_after)
                        .or_else(|| turn.retryable_transport_failure.then_some(None));
                    let Some(retry_after) = retry_after else {
                        return Err(error);
                    };
                    (error, retry_after)
                }
                Err(_) => (
                    LoopError::Provider(
                        "openai-subscription timed out before the first event".into(),
                    ),
                    None,
                ),
            };
            context = turn
                .request_context
                .take()
                .ok_or_else(|| protocol("stream retry context is unavailable"))?;
            context.wire_bytes = turn.wire_bytes;
            if let Some(turn_state) = turn.turn_state.as_deref() {
                context.turn_state = Some(
                    turn_state
                        .parse()
                        .map_err(|_| protocol("validated x-codex-turn-state became invalid"))?,
                );
                context.turn_state_from_header = turn.turn_state_from_header;
            }
            context
                .retry_response_failure(error, RetryDelay::Hint(retry_after), cancellation.clone())
                .await?;
        }
    }

    fn model_name(&self) -> Option<&str> {
        Some(&self.config.model)
    }

    fn provider_name(&self) -> Option<&str> {
        Some("openai-subscription")
    }
}

impl OpenAiSubscriptionSession {
    async fn credentials_with_budget(
        &self,
        rejected: Option<auth::TokenRecord>,
        cancellation: Option<agentkit_core::TurnCancellation>,
        started: tokio::time::Instant,
        attempts: usize,
    ) -> Result<auth::TokenRecord, LoopError> {
        let elapsed = started.elapsed();
        if elapsed >= RETRY_BUDGET {
            return Err(retry_exhausted(
                LoopError::Provider(
                    "openai-subscription credential lookup exceeded retry budget".into(),
                ),
                attempts,
                elapsed,
            ));
        }
        tokio::time::timeout_at(
            (started + RETRY_BUDGET).min(tokio::time::Instant::now() + ATTEMPT_TIMEOUT),
            self.credentials(rejected, cancellation),
        )
        .await
        .map_err(|_| {
            retry_exhausted(
                LoopError::Provider(
                    "openai-subscription credential lookup exceeded retry budget".into(),
                ),
                attempts,
                started.elapsed(),
            )
        })?
    }

    fn ensure_binding(&self, credentials: &auth::TokenRecord) -> Result<(), LoopError> {
        ensure_credential_binding(&self.binding, credentials)
    }

    async fn credentials(
        &self,
        rejected: Option<auth::TokenRecord>,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<auth::TokenRecord, LoopError> {
        #[cfg(test)]
        if let Some(credentials) = &self.test_credentials {
            return Ok(credentials.clone());
        }
        credentials(&self.config, rejected, cancellation).await
    }
}

impl OpenAiSubscriptionRequestContext {
    fn ensure_binding(&self, credentials: &auth::TokenRecord) -> Result<(), LoopError> {
        ensure_credential_binding(&self.binding, credentials)
    }

    async fn credentials(
        &self,
        rejected: Option<auth::TokenRecord>,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<auth::TokenRecord, LoopError> {
        #[cfg(test)]
        if let Some(credentials) = &self.test_credentials {
            return Ok(credentials.clone());
        }
        credentials(&self.config, rejected, cancellation).await
    }

    async fn credentials_with_budget(
        &self,
        rejected: Option<auth::TokenRecord>,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<auth::TokenRecord, LoopError> {
        let now = tokio::time::Instant::now();
        if now >= self.deadline {
            return Err(retry_exhausted(
                LoopError::Provider(
                    "openai-subscription credential lookup exceeded retry budget".into(),
                ),
                self.retries.saturating_add(1),
                self.started.elapsed(),
            ));
        }
        tokio::time::timeout_at(
            self.deadline.min(now + ATTEMPT_TIMEOUT),
            self.credentials(rejected, cancellation),
        )
        .await
        .map_err(|_| {
            retry_exhausted(
                LoopError::Provider(
                    "openai-subscription credential lookup exceeded retry budget".into(),
                ),
                self.retries.saturating_add(1),
                self.started.elapsed(),
            )
        })?
    }

    async fn retry_response_failure(
        &mut self,
        error: LoopError,
        delay: RetryDelay,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<(), LoopError> {
        retry_failure(
            error,
            delay,
            &mut self.retries,
            self.started,
            &mut self.deadline,
            &self.idempotency_key,
            cancellation.clone(),
        )
        .await?;
        self.credentials = self.credentials_with_budget(None, cancellation).await?;
        self.ensure_binding(&self.credentials)
    }

    async fn send_response(
        &mut self,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<reqwest::Response, LoopError> {
        loop {
            let now = tokio::time::Instant::now();
            if now >= self.deadline {
                return Err(retry_exhausted(
                    LoopError::Provider("openai-subscription retry budget exhausted".into()),
                    self.retries.saturating_add(1),
                    self.started.elapsed(),
                ));
            }
            self.ensure_binding(&self.credentials)?;
            let mut builder = self
                .client
                .post(self.config.endpoint())
                .bearer_auth(self.credentials.access_token())
                .header("originator", "kit")
                .header("session-id", &self.session_id)
                .header("Idempotency-Key", &self.idempotency_key)
                .header("Accept", "text/event-stream")
                .header("Content-Type", "application/json");
            if let Some(account_id) = self.credentials.account_id() {
                builder = builder.header("ChatGPT-Account-ID", account_id);
            }
            if let Some(value) = self.turn_state.as_ref() {
                builder = builder.header(X_CODEX_TURN_STATE, value);
            }
            let send = tokio::time::timeout_at(
                self.deadline.min(now + ATTEMPT_TIMEOUT),
                builder.body(self.body_bytes.to_vec()).send(),
            );
            let response = if let Some(cancel) = cancellation.clone() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(LoopError::Cancelled),
                    response = send => response,
                }
            } else {
                send.await
            };
            let response = match response {
                Err(_) => {
                    self.retry_response_failure(
                        LoopError::Provider("openai-subscription request timed out".into()),
                        RetryDelay::Hint(None),
                        cancellation.clone(),
                    )
                    .await?;
                    continue;
                }
                Ok(Ok(response)) => response,
                Ok(Err(error))
                    if retriable_transport_error(crate::fatal::TransportStage::Request, &error) =>
                {
                    self.retry_response_failure(
                        transport_error(
                            crate::fatal::TransportStage::Request,
                            &error,
                            true,
                            self.retries.saturating_add(1),
                            None,
                        ),
                        RetryDelay::Hint(None),
                        cancellation.clone(),
                    )
                    .await?;
                    continue;
                }
                Ok(Err(error)) => {
                    return Err(transport_error(
                        crate::fatal::TransportStage::Request,
                        &error,
                        false,
                        self.retries.saturating_add(1),
                        None,
                    ));
                }
            };
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !self.unauthorized {
                if tokio::time::Instant::now() >= self.deadline {
                    return Err(retry_exhausted(
                        LoopError::Provider(
                            "openai-subscription unauthorized before credential refresh".into(),
                        ),
                        self.retries.saturating_add(1),
                        self.started.elapsed(),
                    ));
                }
                self.retries += 1;
                self.unauthorized = true;
                let rejected = self.credentials.clone();
                self.credentials = self
                    .credentials_with_budget(Some(rejected), cancellation.clone())
                    .await?;
                self.ensure_binding(&self.credentials)?;
                continue;
            }
            if retriable_http_status(response.status()) {
                let status = response.status();
                let reset = (status == reqwest::StatusCode::TOO_MANY_REQUESTS)
                    .then(|| rate_limit_reset(response.headers()))
                    .flatten();
                let delay = match reset {
                    Some(reset) => RetryDelay::RateLimitReset(reset),
                    None => RetryDelay::Hint(retry_after(response.headers())),
                };
                self.retry_response_failure(
                    LoopError::Provider(format!(
                        "openai-subscription returned retryable HTTP {status}"
                    )),
                    delay,
                    cancellation.clone(),
                )
                .await?;
                continue;
            }
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(LoopError::Provider(
                    "openai-subscription unauthorized after one refresh".to_owned(),
                ));
            }
            if !status.is_success() {
                let now = tokio::time::Instant::now();
                let detail = if now < self.deadline {
                    failure_body_excerpt(
                        response,
                        cancellation.clone(),
                        self.deadline.min(now + ATTEMPT_TIMEOUT),
                    )
                    .await?
                } else {
                    String::new()
                };
                return Err(LoopError::Provider(format!(
                    "openai-subscription returned {status}{detail}"
                )));
            }
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
            let turn_state = validated_turn_state_header(response.headers())?;
            self.turn_state_from_header = turn_state.is_some();
            if let Some(turn_state) = turn_state {
                if self
                    .turn_state
                    .as_ref()
                    .is_some_and(|expected| expected != turn_state)
                {
                    return Err(protocol(
                        "provider changed x-codex-turn-state while routing a retry",
                    ));
                }
                self.turn_state = Some(turn_state);
            }
            return Ok(response);
        }
    }
}

fn ensure_credential_binding(
    expected: &auth::CredentialBinding,
    credentials: &auth::TokenRecord,
) -> Result<(), LoopError> {
    let actual = credentials
        .binding()
        .map_err(|error| LoopError::Provider(error.to_string()))?;
    if &actual == expected {
        Ok(())
    } else {
        Err(LoopError::Provider(
            "OpenAI credential account changed; start a new session".to_owned(),
        ))
    }
}

async fn credentials(
    config: &SubscriptionConfig,
    rejected: Option<auth::TokenRecord>,
    cancellation: Option<agentkit_core::TurnCancellation>,
) -> Result<auth::TokenRecord, LoopError> {
    let config = config.clone();
    let worker = tokio::task::spawn_blocking(move || match rejected {
        Some(record) => auth::refresh_after_unauthorized(
            &config.credential_storage,
            record.access_token(),
            Instant::now() + Duration::from_secs(30),
        ),
        None => auth::access_token(
            &config.credential_storage,
            Instant::now() + Duration::from_secs(30),
        ),
    });
    let result = if let Some(cancel) = cancellation {
        tokio::select! {
            biased;
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

async fn fetch_context_windows(
    client: &reqwest::Client,
    credentials: &auth::TokenRecord,
) -> Result<HashMap<String, u64>, LoopError> {
    let endpoint = format!("{MODELS_ENDPOINT}?client_version={MODEL_CATALOG_CLIENT_VERSION}");
    let mut request = client
        .get(endpoint)
        .bearer_auth(credentials.access_token())
        .header("originator", "kit")
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5));
    if let Some(account_id) = credentials.account_id() {
        request = request.header("ChatGPT-Account-ID", account_id);
    }
    let response = request
        .send()
        .await
        .map_err(|_| LoopError::Provider("model catalog transport failed".to_owned()))?;
    if !response.status().is_success() {
        return Err(LoopError::Provider(format!(
            "model catalog returned {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODELS_BYTES as u64)
    {
        return Err(protocol("model catalog exceeds 2 MiB"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| LoopError::Provider("model catalog body failed".to_owned()))?;
        if body.len().saturating_add(chunk.len()) > MAX_MODELS_BYTES {
            return Err(protocol("model catalog exceeds 2 MiB"));
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| protocol("model catalog is not valid JSON"))?;
    parse_context_windows(&value)
}

fn parse_context_windows(value: &Value) -> Result<HashMap<String, u64>, LoopError> {
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .filter(|models| models.len() <= MAX_MODELS)
        .ok_or_else(|| protocol("model catalog omitted a bounded models list"))?;
    let mut windows = HashMap::new();
    for model in models {
        let Some(slug) = model
            .get("slug")
            .and_then(Value::as_str)
            .filter(|slug| valid_model(slug))
        else {
            continue;
        };
        let Some(window) = model
            .get("context_window")
            .filter(|window| !window.is_null())
        else {
            continue;
        };
        let Some(window) = window.as_u64().filter(|window| *window > 0) else {
            continue;
        };
        windows.entry(slug.to_owned()).or_insert(window);
    }
    Ok(windows)
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

struct OpenAiSubscriptionTurnInit {
    requested_model: String,
    header_model: Option<String>,
    turn_state: Option<String>,
    turn_state_from_header: bool,
    binding: auth::CredentialBinding,
    session_id: String,
    context_windows: Arc<HashMap<String, u64>>,
    response_attempt_replacement: bool,
    attempt: usize,
    response_request_id: Option<String>,
    wire_bytes: usize,
    attempt_wire_bytes: usize,
    request_context: Option<OpenAiSubscriptionRequestContext>,
}

pub struct OpenAiSubscriptionTurn {
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
    reasoning: HashMap<(String, u64), PartAccumulator>,
    reasoning_sections: HashSet<(String, u64)>,
    created: bool,
    completed: bool,
    sequence: Option<u64>,
    wire_bytes: usize,
    attempt_wire_bytes: usize,
    usage: Option<Usage>,
    response_id: Option<String>,
    requested_model: String,
    header_model: Option<String>,
    response_model: Option<String>,
    turn_state: Option<String>,
    turn_state_from_header: bool,
    binding: auth::CredentialBinding,
    session_id: String,
    context_windows: Arc<HashMap<String, u64>>,
    tool_call: bool,
    next_media: usize,
    pending_failure: Option<ResponseFailure>,
    retryable_transport_failure: bool,
    response_attempt_replacement: bool,
    model_event_emitted: bool,
    append_text_emitted: bool,
    attempt: usize,
    response_request_id: Option<String>,
    request_context: Option<OpenAiSubscriptionRequestContext>,
    pending_reopen: Option<PendingReopen>,
}

#[derive(Debug)]
struct PendingReopen {
    error: LoopError,
    retry_after: Option<Duration>,
}

#[derive(Clone, Debug)]
struct ResponseFailure {
    message: String,
    retriable: bool,
    retry_after: Option<Duration>,
}

impl ResponseFailure {
    fn into_error(self) -> LoopError {
        LoopError::Provider(self.message)
    }
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
            OpenAiSubscriptionTurnInit {
                requested_model,
                header_model: actual_model,
                turn_state: None,
                turn_state_from_header: false,
                binding: auth::CredentialBinding {
                    account_id: "test-account".to_owned(),
                    generation: "test-generation".to_owned(),
                },
                session_id: "s".to_owned(),
                context_windows: Arc::new(HashMap::new()),
                response_attempt_replacement: false,
                attempt: 1,
                response_request_id: None,
                wire_bytes: 0,
                attempt_wire_bytes: 0,
                request_context: None,
            },
        )
    }

    fn new_inner(
        stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        init: OpenAiSubscriptionTurnInit,
    ) -> Self {
        let OpenAiSubscriptionTurnInit {
            requested_model,
            header_model,
            turn_state,
            turn_state_from_header,
            binding,
            session_id,
            context_windows,
            response_attempt_replacement,
            attempt,
            response_request_id,
            wire_bytes,
            attempt_wire_bytes,
            request_context,
        } = init;
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
            reasoning: HashMap::new(),
            reasoning_sections: HashSet::new(),
            created: false,
            completed: false,
            sequence: None,
            wire_bytes,
            attempt_wire_bytes,
            usage: None,
            response_id: None,
            requested_model,
            header_model,
            response_model: None,
            turn_state,
            turn_state_from_header,
            binding,
            session_id,
            context_windows,
            tool_call: false,
            next_media: 0,
            pending_failure: None,
            retryable_transport_failure: false,
            response_attempt_replacement,
            model_event_emitted: false,
            append_text_emitted: false,
            attempt,
            response_request_id,
            request_context,
            pending_reopen: None,
        }
    }

    fn schedule_reopen(
        &mut self,
        error: LoopError,
        retry_after: Option<Duration>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        if self.request_context.is_none()
            || (self.model_event_emitted && !self.response_attempt_replacement)
        {
            return Err(error);
        }
        let marker_required = self.append_text_emitted;
        self.pending_reopen = Some(PendingReopen { error, retry_after });
        Ok(marker_required.then(crate::response_attempt::marker_event))
    }

    async fn reopen_stream(
        &mut self,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<(), LoopError> {
        let PendingReopen { error, retry_after } = self
            .pending_reopen
            .take()
            .ok_or_else(|| protocol("stream retry action is unavailable"))?;
        self.retryable_transport_failure = false;
        let parsed_turn_state = self
            .turn_state
            .as_deref()
            .map(|value| {
                value
                    .parse::<reqwest::header::HeaderValue>()
                    .map_err(|_| protocol("validated x-codex-turn-state became invalid"))
            })
            .transpose()?;
        let wire_bytes = self.wire_bytes;
        let context = self
            .request_context
            .as_mut()
            .ok_or_else(|| protocol("stream retry context is unavailable"))?;
        context.wire_bytes = wire_bytes;
        if let Some(turn_state) = parsed_turn_state {
            context.turn_state = Some(turn_state);
        }
        context
            .retry_response_failure(error, RetryDelay::Hint(retry_after), cancellation.clone())
            .await?;
        let response = context.send_response(cancellation).await?;
        let attempt = context.retries.saturating_add(1);
        let turn_state = context
            .turn_state
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let turn_state_from_header = context.turn_state_from_header;
        let header_model = response
            .headers()
            .get("openai-model")
            .map(validated_model_header)
            .transpose()?;
        let response_request_id = crate::fatal::safe_response_request_id(response.headers());
        self.reset_attempt_state();
        self.header_model = header_model;
        self.turn_state_from_header = turn_state_from_header;
        self.turn_state = turn_state;
        self.attempt = attempt;
        self.response_request_id = response_request_id;
        self.stream = Box::pin(response.bytes_stream());
        Ok(())
    }

    fn consume_frame(&mut self, frame: &[u8]) -> Result<(), LoopError> {
        if frame.len() > MAX_EVENT_BYTES {
            return Err(protocol("SSE event exceeds the canonical limit"));
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
        if self.pending_failure.is_some() {
            return Err(protocol("event followed terminal provider error"));
        }
        match kind {
            "response.created" => {
                if self.created {
                    return Err(protocol("duplicate response.created"));
                }
                let id = bounded_id(value.pointer("/response/id"))?;
                self.response_id = Some(id.to_owned());
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
                let delta = bounded_string(value, "delta")?;
                let item = event_item(value, "summary_index", &self.item_indices)?;
                self.append_reasoning_delta(item, delta)?;
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
            "response.completed" => {
                let result = self.complete(value);
                if result.is_err() {
                    self.clear_retry_state();
                }
                result?;
            }
            "response.incomplete" => {
                let result = self.incomplete(value);
                if result.is_err() {
                    self.clear_retry_state();
                }
                result?;
            }
            "response.failed" => {
                if let Some(id) = value.pointer("/response/id") {
                    let id = bounded_id(Some(id))?;
                    if self.response_id.as_deref() != Some(id) {
                        return Err(protocol("response.failed changed the response.created ID"));
                    }
                }
                self.output.retain(|item| {
                    item.parts
                        .iter()
                        .all(|part| !matches!(part, Part::ToolCall(_)))
                });
                self.pending_failure = Some(classify_response_failure(value));
            }
            "error" => {
                self.pending_failure = Some(classify_top_level_error(value));
            }
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.in_progress" => {
                self.require_created()?;
            }
            "response.metadata" => {
                self.require_created()?;
                if let Some(model) = responses_header(value, &["openai-model", "x-openai-model"]) {
                    self.observe_model(&Value::String(model.to_owned()))?;
                }
                if let Some(state) = responses_turn_state(value)? {
                    match self.turn_state.as_deref() {
                        Some(expected) if expected != state => {
                            return Err(protocol("response metadata changed x-codex-turn-state"));
                        }
                        None => self.turn_state = Some(state.to_owned()),
                        _ => {}
                    }
                    if let Some(context) = self.request_context.as_mut() {
                        context.turn_state =
                            Some(state.parse().map_err(|_| {
                                protocol("response metadata turn state is invalid")
                            })?);
                    }
                }
            }
            "keepalive" => {}
            _ => {}
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
                let call_id = bounded_nonempty_string(item, "call_id")?;
                if !self.seen_call_ids.insert(call_id.to_owned()) {
                    return Err(protocol("duplicate function call ID"));
                }
                let name = bounded_nonempty_string(item, "name")?;
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
            Some("image_generation_call") => {
                let item_id = bounded_id(item.get("id"))?;
                let status = bounded_nonempty_string(item, "status")?;
                if status != "completed" {
                    return Err(protocol("image generation did not complete"));
                }
                let result = item
                    .get("result")
                    .and_then(Value::as_str)
                    .filter(|result| !result.is_empty() && result.len() <= MAX_EVENT_BYTES)
                    .ok_or_else(|| {
                        protocol("generated image result is outside canonical bounds")
                    })?;
                let bytes = STANDARD
                    .decode(result)
                    .map_err(|_| protocol("generated image result is not valid base64"))?;
                if bytes.is_empty() {
                    return Err(protocol("generated image result is empty"));
                }
                let revised_prompt = item
                    .get("revised_prompt")
                    .filter(|value| !value.is_null())
                    .map(|_| bounded_string(item, "revised_prompt"))
                    .transpose()?;
                let mut metadata = continuation_metadata(
                    &self.binding,
                    &self.requested_model,
                    &self.session_id,
                    item_id,
                    output_index,
                    "image_generation_call",
                    None,
                );
                metadata.insert(
                    GENERATED_IMAGE_METADATA.to_owned(),
                    json!({
                        "item_id": item_id,
                        "status": status,
                        "revised_prompt": revised_prompt,
                    }),
                );
                let media =
                    MediaPart::new(Modality::Image, "image/png", DataRef::InlineBytes(bytes))
                        .with_metadata(metadata);
                self.next_media += 1;
                let placeholder = format!("[Image #{}]", self.next_media);
                let placeholder_id = PartId::new(format!("generated-image-{output_index}"));
                self.queued
                    .push_back(ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id: placeholder_id.clone(),
                        kind: PartKind::Text,
                    }));
                self.queued
                    .push_back(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: placeholder_id,
                        chunk: placeholder,
                    }));
                self.push_output(
                    output_index,
                    Item::new(ItemKind::Assistant, vec![Part::Media(media)]),
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
                let item_id = bounded_id(item.get("id"))?;
                let mut summary_texts = Vec::with_capacity(summaries.len());
                for (index, summary) in summaries.iter().enumerate() {
                    let text = bounded_string(summary, "text")?;
                    self.commit_reasoning_part(item_id, index as u64, text)?;
                    summary_texts.push(text);
                }
                if self.reasoning.keys().any(|(id, _)| id == item_id) {
                    return Err(protocol(
                        "completed reasoning omitted streamed summary content",
                    ));
                }
                self.reasoning_sections.retain(|(id, _)| id != item_id);
                let summary = (!summary_texts.is_empty()).then(|| summary_texts.join("\n\n"));
                let metadata = continuation_metadata(
                    &self.binding,
                    &self.requested_model,
                    &self.session_id,
                    item_id,
                    output_index,
                    "reasoning",
                    Some(encrypted_content),
                );
                self.push_output(
                    output_index,
                    Item::new(
                        ItemKind::Assistant,
                        vec![Part::Reasoning(ReasoningPart {
                            summary,
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
        if self.seen_ids != self.done_ids || !self.text.is_empty() || !self.reasoning.is_empty() {
            return Err(protocol(
                "response.completed preceded complete output items",
            ));
        }
        let response = value
            .get("response")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol("response.completed omitted response"))?;
        let id = bounded_id(response.get("id"))?;
        if self.response_id.as_deref() != Some(id) {
            return Err(protocol(
                "response.completed changed the response.created ID",
            ));
        }
        self.finalize_continuation_metadata(id)?;
        if let Some(model) = response.get("model") {
            self.observe_model(model)?;
        }
        if let Some(raw) = response.get("usage") {
            let usage = parse_usage(raw, self.context_window())?;
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
        let mut metadata =
            model_metadata(self.header_model.as_deref(), self.response_model.as_deref());
        set_provider_finish_reasons(&mut metadata, ["completed"]);
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
        self.clear_retry_state();
        Ok(())
    }

    fn incomplete(&mut self, value: &Value) -> Result<(), LoopError> {
        self.require_created()?;
        let response = value
            .get("response")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol("response.incomplete omitted response"))?;
        let id = bounded_id(response.get("id"))?;
        if self.response_id.as_deref() != Some(id) {
            return Err(protocol(
                "response.incomplete changed the response.created ID",
            ));
        }
        if let Some(model) = response.get("model") {
            self.observe_model(model)?;
        }
        if let Some(raw) = response.get("usage") {
            let usage = parse_usage(raw, self.context_window())?;
            self.usage = Some(usage.clone());
            self.queued.push_back(ModelTurnEvent::Usage(usage));
        }
        self.flush_partial_output();
        for item in &mut self.output {
            item.parts.retain(|part| matches!(part, Part::Text(_)));
        }
        self.output.retain(|item| !item.parts.is_empty());
        self.reasoning_sections.clear();
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
        let mut metadata =
            model_metadata(self.header_model.as_deref(), self.response_model.as_deref());
        set_provider_finish_reasons(&mut metadata, [reason]);
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
        self.clear_retry_state();
        Ok(())
    }

    fn clear_retry_state(&mut self) {
        self.request_context = None;
    }

    fn reset_attempt_state(&mut self) {
        self.buffer.zeroize();
        self.queued.clear();
        self.output.clear();
        self.output_indices.clear();
        self.seen_ids.clear();
        self.seen_call_ids.clear();
        self.done_ids.clear();
        self.item_indices.clear();
        self.text.clear();
        self.reasoning.clear();
        self.reasoning_sections.clear();
        self.created = false;
        self.completed = false;
        self.sequence = None;
        self.attempt_wire_bytes = 0;
        self.usage = None;
        self.response_id = None;
        self.header_model = None;
        self.response_model = None;
        self.tool_call = false;
        self.next_media = 0;
        self.pending_failure = None;
        self.retryable_transport_failure = false;
        self.model_event_emitted = false;
        self.append_text_emitted = false;
        self.response_request_id = None;
    }

    fn clear_error_state(&mut self) {
        self.clear_retry_state();
        self.buffer.zeroize();
    }

    fn flush_partial_output(&mut self) {
        let mut partial = self
            .text
            .drain()
            .map(|((id, index), part)| (id, index, false, part))
            .chain(
                self.reasoning
                    .drain()
                    .map(|((id, index), part)| (id, index, true, part)),
            )
            .collect::<Vec<_>>();
        partial.sort_by_key(|(id, index, reasoning, _)| {
            (
                self.item_indices.get(id).copied().unwrap_or(u64::MAX),
                *index,
                *reasoning,
            )
        });
        for (id, _, reasoning, part) in partial {
            let output = if reasoning {
                Part::Reasoning(ReasoningPart {
                    summary: Some(part.text),
                    data: None,
                    redacted: true,
                    metadata: MetadataMap::new(),
                })
            } else {
                Part::Text(TextPart::new(part.text))
            };
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
                Part::Media(part) if part.metadata.contains_key(GENERATED_IMAGE_METADATA) => {
                    &mut part.metadata
                }
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

    fn append_reasoning_delta(
        &mut self,
        item: (String, u64),
        delta: &str,
    ) -> Result<(), LoopError> {
        if self.reasoning_sections.insert(item.clone()) && item.1 > 0 {
            append_part(
                &mut self.reasoning,
                item.clone(),
                "\n\n",
                PartKind::Reasoning,
                &mut self.queued,
            )?;
        }
        append_part(
            &mut self.reasoning,
            item,
            delta,
            PartKind::Reasoning,
            &mut self.queued,
        )
    }

    fn commit_reasoning_part(
        &mut self,
        item_id: &str,
        index: u64,
        completed: &str,
    ) -> Result<(), LoopError> {
        if let Some(streamed) = self.reasoning.remove(&(item_id.to_owned(), index)) {
            let expected = if index == 0 {
                completed.to_owned()
            } else {
                format!("\n\n{completed}")
            };
            if streamed.text != expected {
                return Err(protocol(
                    "completed reasoning differs from streamed summary deltas",
                ));
            }
            self.queued
                .push_back(ModelTurnEvent::Delta(Delta::CommitPart {
                    part: Part::Reasoning(ReasoningPart {
                        summary: Some(expected),
                        data: None,
                        redacted: true,
                        metadata: MetadataMap::new(),
                    }),
                }));
        }
        Ok(())
    }

    fn context_window(&self) -> Option<u64> {
        let model = self
            .header_model
            .as_ref()
            .or(self.response_model.as_ref())
            .unwrap_or(&self.requested_model);
        self.context_windows.get(model).copied()
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

impl OpenAiSubscriptionTurn {
    async fn next_event_inner(
        &mut self,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        loop {
            let marker_required = self.append_text_emitted
                && self.response_attempt_replacement
                && self.request_context.is_some()
                && self
                    .pending_failure
                    .as_ref()
                    .is_some_and(|failure| failure.retriable);
            if marker_required {
                let failure = self
                    .pending_failure
                    .take()
                    .expect("marker requirement came from a pending failure");
                let retry_after = failure.retry_after;
                if let Some(marker) = self.schedule_reopen(failure.into_error(), retry_after)? {
                    return Ok(Some(marker));
                }
                unreachable!("visible output always requires a replacement marker");
            }
            if self.pending_reopen.is_some() {
                self.reopen_stream(cancellation.clone()).await?;
                continue;
            }
            if cancellation
                .as_ref()
                .is_some_and(|value| value.is_cancelled())
            {
                return Err(LoopError::Cancelled);
            }
            if let Some(event) = self.queued.pop_front() {
                self.model_event_emitted = true;
                if matches!(&event, ModelTurnEvent::Delta(Delta::AppendText { .. })) {
                    self.append_text_emitted = true;
                }
                if !self.response_attempt_replacement {
                    self.clear_retry_state();
                }
                return Ok(Some(event));
            }
            if let Some(failure) = self.pending_failure.take() {
                let retriable = failure.retriable;
                let retry_after = failure.retry_after;
                let error = failure.into_error();
                if !retriable {
                    return Err(error);
                }
                if let Some(marker) = self.schedule_reopen(error, retry_after)? {
                    return Ok(Some(marker));
                }
                self.reopen_stream(cancellation.clone()).await?;
                continue;
            }
            if self.completed {
                return Ok(None);
            }
            let read = tokio::time::timeout(STREAM_IDLE_TIMEOUT, self.stream.next());
            let next = if let Some(cancel) = cancellation.clone() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(LoopError::Cancelled),
                    value = read => value,
                }
            } else {
                read.await
            };
            let next = match next {
                Ok(next) => next,
                Err(_) => {
                    self.retryable_transport_failure = true;
                    let error =
                        LoopError::Provider("openai-subscription SSE idle timeout".to_owned());
                    if let Some(marker) = self.schedule_reopen(error, None)? {
                        return Ok(Some(marker));
                    }
                    self.reopen_stream(cancellation.clone()).await?;
                    continue;
                }
            };
            let Some(chunk) = next else {
                self.retryable_transport_failure = true;
                let error = protocol("SSE stream closed before response.completed");
                if let Some(marker) = self.schedule_reopen(error, None)? {
                    return Ok(Some(marker));
                }
                self.reopen_stream(cancellation.clone()).await?;
                continue;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let retryable =
                        retriable_transport_error(crate::fatal::TransportStage::Stream, &error);
                    self.retryable_transport_failure = retryable;
                    let error = transport_error(
                        crate::fatal::TransportStage::Stream,
                        &error,
                        retryable,
                        self.attempt,
                        self.response_request_id.as_deref(),
                    );
                    if !retryable {
                        return Err(error);
                    }
                    if let Some(marker) = self.schedule_reopen(error, None)? {
                        return Ok(Some(marker));
                    }
                    self.reopen_stream(cancellation.clone()).await?;
                    continue;
                }
            };
            self.attempt_wire_bytes = self
                .attempt_wire_bytes
                .checked_add(chunk.len())
                .ok_or_else(|| protocol("per-attempt SSE wire ingress overflowed"))?;
            if self.attempt_wire_bytes > MAX_STREAM_BYTES {
                return Err(protocol("per-attempt SSE wire ingress exceeds 16 MiB"));
            }
            self.wire_bytes = self
                .wire_bytes
                .checked_add(chunk.len())
                .ok_or_else(|| protocol("aggregate SSE wire ingress overflowed"))?;
            if self.wire_bytes > MAX_WIRE_BYTES {
                return Err(protocol("aggregate SSE wire ingress exceeds 64 MiB"));
            }
            zeroizing_extend(
                &mut self.buffer,
                &chunk,
                MAX_EVENT_BYTES,
                "SSE event exceeds the canonical limit",
            )?;
            while let Some((end, delimiter)) = frame_end(&self.buffer) {
                let frame = Zeroizing::new(self.buffer[..end].to_vec());
                self.buffer.drain(..end + delimiter);
                if !frame.is_empty() {
                    self.consume_frame(&frame)?;
                }
                if self.pending_failure.is_some() {
                    self.buffer.zeroize();
                    break;
                }
            }
            if self.completed && !self.buffer.is_empty() {
                return Err(protocol("terminal response was followed by trailing bytes"));
            }
            if self.buffer.len() > MAX_EVENT_BYTES {
                return Err(protocol("SSE event exceeds the canonical limit"));
            }
        }
    }
}

#[async_trait]
impl ModelTurn for OpenAiSubscriptionTurn {
    async fn next_event(
        &mut self,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        let result = self.next_event_inner(cancellation).await;
        if result.is_err() {
            self.clear_error_state();
        }
        result
    }
}

fn request_body(
    model: &str,
    reasoning_effort: Option<super::adapter::ReasoningEffort>,
    request: &TurnRequest,
    binding: &auth::CredentialBinding,
    session_id: &str,
) -> Result<Value, LoopError> {
    if request.transcript.len() > MAX_ITEMS || request.available_tools.len() > MAX_ITEMS {
        return Err(protocol(
            "request contains too many transcript items or tools",
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
    let mut reasoning = json!({"summary": "auto"});
    if let Some(reasoning_effort) = reasoning_effort {
        reasoning["effort"] = json!(reasoning_effort.as_str());
    }
    let mut body = json!({
        "model": model, "input": input, "tools": tools, "tool_choice": "auto",
        "parallel_tool_calls": true, "reasoning": reasoning, "store": false,
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
            Part::Media(media) => {
                if let Some(generated) = generated_image_item(media, continuation)? {
                    if role != "assistant" {
                        return Err(protocol(
                            "generated image metadata appeared outside assistant output",
                        ));
                    }
                    messages.push(generated);
                } else {
                    if role == "assistant" || role == "tool" {
                        return Err(LoopError::Unsupported(
                            "openai-subscription assistant/tool message contains unsupported media"
                                .to_owned(),
                        ));
                    }
                    content.push(media_input(media)?);
                }
            }
            Part::File(_) | Part::Custom(_) => return Err(LoopError::Unsupported("openai-subscription transcript contains unsupported content".to_owned())),
        }
    }
    if !content.is_empty() && role != "tool" {
        messages.insert(0, json!({"type":"message","role":role,"content":content}));
    }
    Ok(messages)
}

fn media_input(media: &MediaPart) -> Result<Value, LoopError> {
    let expected_prefix = match media.modality {
        Modality::Image => "image/",
        Modality::Audio => "audio/",
        Modality::Video => {
            return Err(LoopError::Unsupported(
                "openai-subscription does not support video input".to_owned(),
            ));
        }
        Modality::Binary => {
            return Err(LoopError::Unsupported(
                "openai-subscription does not support binary media input".to_owned(),
            ));
        }
    };
    if !media.mime_type.starts_with(expected_prefix)
        || media.mime_type.contains(['\r', '\n', ';', ','])
    {
        return Err(LoopError::Unsupported(
            "openai-subscription media has an invalid MIME type".to_owned(),
        ));
    }
    let data_url = media_data_url(media)?;
    Ok(match media.modality {
        Modality::Image => json!({
            "type": "input_image",
            "image_url": data_url,
            "detail": "high",
        }),
        Modality::Audio => json!({"type": "input_audio", "audio_url": data_url}),
        Modality::Video | Modality::Binary => unreachable!("rejected above"),
    })
}

fn media_data_url(media: &MediaPart) -> Result<String, LoopError> {
    match &media.data {
        DataRef::InlineBytes(bytes) => Ok(format!(
            "data:{};base64,{}",
            media.mime_type,
            STANDARD.encode(bytes)
        )),
        DataRef::InlineText(text) => {
            if text.starts_with("data:") {
                validate_data_url(text, &media.mime_type)?;
                Ok(text.clone())
            } else {
                STANDARD.decode(text).map_err(|_| {
                    LoopError::Unsupported(
                        "openai-subscription inline media is not valid base64".to_owned(),
                    )
                })?;
                Ok(format!("data:{};base64,{text}", media.mime_type))
            }
        }
        DataRef::Uri(uri) if uri.starts_with("data:") => {
            validate_data_url(uri, &media.mime_type)?;
            Ok(uri.clone())
        }
        DataRef::Uri(uri)
            if media.modality == Modality::Image
                && uri.len() <= MAX_TEXT_BYTES
                && url::Url::parse(uri)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https")) =>
        {
            Ok(uri.clone())
        }
        DataRef::Uri(_) => Err(LoopError::Unsupported(
            "openai-subscription cannot read this media URI; provide inline bytes".to_owned(),
        )),
        DataRef::Handle(_) => Err(LoopError::Unsupported(
            "openai-subscription cannot resolve media handles; provide inline bytes".to_owned(),
        )),
    }
}

fn validate_data_url(value: &str, mime_type: &str) -> Result<(), LoopError> {
    let payload = value
        .strip_prefix(&format!("data:{mime_type};base64,"))
        .filter(|payload| !payload.is_empty())
        .ok_or_else(|| {
            LoopError::Unsupported(
                "openai-subscription media data URL is not canonical base64".to_owned(),
            )
        })?;
    STANDARD.decode(payload).map_err(|_| {
        LoopError::Unsupported("openai-subscription media data URL is not valid base64".to_owned())
    })?;
    Ok(())
}

fn generated_image_item(
    media: &MediaPart,
    continuation: Option<&ContinuationContext<'_>>,
) -> Result<Option<Value>, LoopError> {
    let Some(metadata) = media.metadata.get(GENERATED_IMAGE_METADATA) else {
        return Ok(None);
    };
    let metadata = metadata
        .as_object()
        .filter(|metadata| (2..=3).contains(&metadata.len()))
        .ok_or_else(|| protocol("generated image metadata is malformed"))?;
    let item_id = bounded_id(metadata.get("item_id"))?;
    if metadata.get("status").and_then(Value::as_str) != Some("completed")
        || media.modality != Modality::Image
        || media.mime_type != "image/png"
    {
        return Err(protocol("generated image metadata is invalid"));
    }
    let Some(continuation) = continuation else {
        return Ok(None);
    };
    if continuation_item(&media.metadata, "image_generation_call", continuation)?.is_none() {
        return Ok(None);
    }
    let revised_prompt = metadata
        .get("revised_prompt")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .filter(|value| value.len() <= MAX_TEXT_BYTES)
                .ok_or_else(|| protocol("generated image revised prompt is invalid"))
        })
        .transpose()?;
    let result = match &media.data {
        DataRef::InlineBytes(bytes) if !bytes.is_empty() => STANDARD.encode(bytes),
        DataRef::InlineText(text) if !text.is_empty() && !text.starts_with("data:") => {
            STANDARD
                .decode(text)
                .map_err(|_| protocol("persisted generated image result is not valid base64"))?;
            text.clone()
        }
        DataRef::InlineText(text) => {
            validate_data_url(text, "image/png")?;
            text.split_once(',')
                .map(|(_, payload)| payload.to_owned())
                .ok_or_else(|| protocol("persisted generated image data URL is malformed"))?
        }
        DataRef::Uri(_) | DataRef::Handle(_) | DataRef::InlineBytes(_) => {
            return Err(LoopError::Unsupported(
                "openai-subscription cannot replay generated image without inline bytes".to_owned(),
            ));
        }
    };
    Ok(Some(json!({
        "id": item_id,
        "type": "image_generation_call",
        "status": "completed",
        "revised_prompt": revised_prompt,
        "result": result,
    })))
}

fn tool_output(output: &ToolOutput) -> Result<Value, LoopError> {
    match output {
        ToolOutput::Text(value) => Ok(Value::String(value.clone())),
        ToolOutput::Structured(value) => serde_json::to_string(value)
            .map(Value::String)
            .map_err(|_| protocol("tool output encoding failed")),
        ToolOutput::Parts(parts) if parts.iter().any(|part| matches!(part, Part::Media(_))) => {
            parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => Ok(json!({"type": "input_text", "text": text.text})),
                    Part::Structured(value) => Ok(json!({
                        "type": "input_text",
                        "text": serde_json::to_string(&value.value)
                            .map_err(|_| protocol("tool output encoding failed"))?,
                    })),
                    Part::Media(media) => media_input(media),
                    _ => Err(LoopError::Unsupported(
                        "openai-subscription tool output contains unsupported content".to_owned(),
                    )),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
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
            .map(|parts| Value::String(parts.join("\n"))),
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

#[cfg(any())]
pub(crate) fn durable_reasoning(part: &ReasoningPart) -> bool {
    part.summary.is_none()
        && part.data.is_none()
        && part.redacted
        && validate_continuation_metadata(&part.metadata, "reasoning").is_ok()
}

#[cfg(any())]
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
            "account_id_digest": hex_digest(blake3::hash(binding.account_id.as_bytes()).as_bytes()),
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
    let current_digest = hex_digest(blake3::hash(context.binding.account_id.as_bytes()).as_bytes());
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
        hex_digest(blake3::hash(&bytes).as_bytes())
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

fn classify_response_failure(value: &Value) -> ResponseFailure {
    let error = value.pointer("/response/error").unwrap_or(&Value::Null);
    classify_provider_error(error, Some(value), "response_failed")
}

fn classify_top_level_error(value: &Value) -> ResponseFailure {
    let error = value.get("error").unwrap_or(value);
    classify_provider_error(error, Some(value), "error")
}

fn classify_provider_error(
    error: &Value,
    envelope: Option<&Value>,
    fallback_code: &str,
) -> ResponseFailure {
    let code = canonical_error_field(error.get("code")).unwrap_or(fallback_code);
    let error_type = canonical_error_field(error.get("type")).unwrap_or("unknown");
    let status = error
        .get("status")
        .or_else(|| envelope.and_then(|value| value.get("status")))
        .or_else(|| envelope.and_then(|value| value.pointer("/response/status_code")))
        .and_then(Value::as_u64);
    let retry_after = error
        .get("retry_after")
        .or_else(|| envelope.and_then(|value| value.get("retry_after")))
        .or_else(|| envelope.and_then(|value| value.pointer("/response/retry_after")))
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str()?.parse::<u64>().ok())
        })
        .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_HINT));
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
    let permanent = status.is_some_and(|status| {
        matches!(
            status,
            400 | 402 | 403 | 404 | 405 | 406 | 410 | 413 | 415 | 422 | 501 | 505
        )
    }) || [code, error_type].iter().any(|value| {
        [
            "billing",
            "content_policy",
            "deactivated",
            "insufficient",
            "invalid",
            "not_found",
            "not_supported",
            "permission",
            "quota",
            "unsupported",
        ]
        .iter()
        .any(|marker| value.contains(marker))
    });
    let retriable = !authentication
        && !permanent
        && match status {
            Some(status) => retriable_status_code(status),
            None => true,
        };
    let message = if authentication {
        "openai-subscription authentication failed after inference acceptance".to_owned()
    } else if retriable {
        format!("openai-subscription transient response failed: {error_type}/{code}")
    } else {
        format!("openai-subscription response failed: {error_type}/{code}")
    };
    ResponseFailure {
        message,
        retriable,
        retry_after,
    }
}

fn canonical_error_field(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128 && value.is_ascii())
}

fn zeroize_encrypted_content(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(encrypted)) = object.get_mut("encrypted_content") {
                encrypted.zeroize();
            }
            if object.get("type").and_then(Value::as_str) == Some("image_generation_call")
                && let Some(Value::String(result)) = object.get_mut("result")
            {
                result.zeroize();
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

fn responses_header<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    [value.pointer("/response/headers"), value.get("headers")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .find_map(|headers| {
            headers.iter().find_map(|(name, value)| {
                names
                    .iter()
                    .any(|expected| name.eq_ignore_ascii_case(expected))
                    .then(|| match value {
                        Value::String(value) => Some(value.as_str()),
                        Value::Array(values) => values.first().and_then(Value::as_str),
                        _ => None,
                    })
                    .flatten()
            })
        })
}

fn validated_turn_state_header(
    headers: &reqwest::header::HeaderMap,
) -> Result<Option<reqwest::header::HeaderValue>, LoopError> {
    let mut state = None;
    for value in headers.get_all(X_CODEX_TURN_STATE) {
        if state.as_ref().is_some_and(|expected| expected != value) {
            return Err(protocol(
                "provider returned conflicting x-codex-turn-state headers",
            ));
        }
        state = Some(value.clone());
    }
    Ok(state)
}

fn responses_turn_state(value: &Value) -> Result<Option<&str>, LoopError> {
    let mut state = None;
    for headers in [value.pointer("/response/headers"), value.get("headers")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        for raw in headers.iter().filter_map(|(name, value)| {
            name.eq_ignore_ascii_case(X_CODEX_TURN_STATE)
                .then_some(value)
        }) {
            let values: Vec<&str> = match raw {
                Value::String(value) => vec![value],
                Value::Array(values) if !values.is_empty() => values
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| protocol("response metadata turn state is invalid"))?,
                _ => return Err(protocol("response metadata turn state is invalid")),
            };
            for value in values {
                value
                    .parse::<reqwest::header::HeaderValue>()
                    .map_err(|_| protocol("response metadata turn state is invalid"))?;
                if state.is_some_and(|expected| expected != value) {
                    return Err(protocol("response metadata changed x-codex-turn-state"));
                }
                state = Some(value);
            }
        }
    }
    Ok(state)
}

fn bounded_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, LoopError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_FIELD_BYTES)
        .ok_or_else(|| protocol(&format!("Responses {field} is missing or outside bounds")))
}

fn bounded_nonempty_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, LoopError> {
    bounded_string(value, field).and_then(|value| {
        if value.is_empty() {
            Err(protocol(&format!(
                "Responses {field} is missing or outside bounds"
            )))
        } else {
            Ok(value)
        }
    })
}

fn nonnegative(value: &Value, field: &str) -> Result<u64, LoopError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| protocol("token usage is missing or invalid"))
}

fn zeroizing_extend(
    buffer: &mut Zeroizing<Vec<u8>>,
    bytes: &[u8],
    limit: usize,
    limit_message: &'static str,
) -> Result<(), LoopError> {
    let new_len = buffer
        .len()
        .checked_add(bytes.len())
        .filter(|length| *length <= limit)
        .ok_or_else(|| protocol(limit_message))?;
    if new_len > buffer.capacity() {
        // Vec growth can release the old allocation without wiping it. Move the
        // live prefix ourselves, then zeroize the old allocation before release.
        let capacity = buffer.capacity().saturating_mul(2).max(new_len).min(limit);
        let mut replacement = Vec::with_capacity(capacity);
        replacement.extend_from_slice(buffer);
        let mut previous = std::mem::replace(&mut **buffer, replacement);
        previous.zeroize();
    }
    buffer.extend_from_slice(bytes);
    Ok(())
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

fn parse_usage(value: &Value, context_window: Option<u64>) -> Result<Usage, LoopError> {
    let total_input = nonnegative(value, "input_tokens")?;
    let total_output = nonnegative(value, "output_tokens")?;
    let cached = optional_usage(value.pointer("/input_tokens_details/cached_tokens"))?;
    let cache_write = optional_usage(value.pointer("/input_tokens_details/cache_write_tokens"))?;
    let reasoning = optional_usage(value.pointer("/output_tokens_details/reasoning_tokens"))?;
    if cached.is_some_and(|value| value > total_input)
        || cache_write.is_some_and(|value| value > total_input)
    {
        return Err(protocol("input token detail exceeds total input tokens"));
    }
    if reasoning.is_some_and(|value| value > total_output) {
        return Err(protocol("reasoning tokens exceed total output tokens"));
    }
    let context_used = total_input
        .checked_add(total_output)
        .ok_or_else(|| protocol("total context token usage overflowed"))?;

    // Cached and reasoning tokens are subsets of the provider's totals. Keep
    // the totals as the context numerator and expose the categories separately.
    let mut tokens = TokenUsage::new(total_input, total_output);
    if let Some(cached) = cached {
        tokens = tokens.with_cached_input_tokens(cached);
    }
    if let Some(cache_write) = cache_write {
        tokens = tokens.with_cache_write_input_tokens(cache_write);
    }
    if let Some(reasoning) = reasoning {
        tokens = tokens.with_reasoning_tokens(reasoning);
    }
    let mut metadata = MetadataMap::new();
    metadata.insert("context_used".to_owned(), json!(context_used));
    if let Some(context_window) = context_window {
        metadata.insert("context_window".to_owned(), json!(context_window));
    }
    Ok(Usage::new(tokens).with_metadata(metadata))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryDelay {
    Hint(Option<Duration>),
    RateLimitReset(Duration),
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
    Some(Duration::from_secs(seconds).min(MAX_RETRY_HINT))
}

fn rate_limit_reset(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .iter()
        .filter(|(name, _)| name.as_str().starts_with("x-ratelimit-reset"))
        .filter_map(|(_, value)| parse_rate_limit_reset(value.to_str().ok()?))
        .max()
        .map(|reset| reset.min(RETRY_BUDGET))
}

fn parse_rate_limit_reset(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(number) = value.parse::<f64>() {
        if !number.is_finite() || number < 0.0 {
            return None;
        }
        let unix_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        let seconds = if number >= 1e12 {
            number / 1000.0 - unix_now
        } else if number >= 1e9 {
            number - unix_now
        } else {
            number
        };
        return duration_from_secs(seconds.max(0.0));
    }
    parse_unit_duration(value)
}

fn duration_from_secs(seconds: f64) -> Option<Duration> {
    (seconds.is_finite() && seconds >= 0.0)
        .then(|| Duration::from_secs_f64(seconds.min(RETRY_BUDGET.as_secs_f64())))
}

fn parse_unit_duration(value: &str) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut rest = value;
    while !rest.is_empty() {
        let number_len = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .filter(|len| *len > 0)?;
        let number = rest[..number_len].parse::<f64>().ok()?;
        rest = &rest[number_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit_len] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => return None,
        };
        total = total.saturating_add(duration_from_secs(number * seconds)?);
        rest = &rest[unit_len..];
    }
    (!value.is_empty()).then_some(total)
}

fn retry_backoff(idempotency_key: &str, retry_number: usize) -> Duration {
    let exponent = retry_number.saturating_sub(1).min(5) as u32;
    let cap = Duration::from_secs(1_u64 << exponent).min(MAX_RETRY_BACKOFF);
    let mut input = Vec::with_capacity(idempotency_key.len() + std::mem::size_of::<usize>());
    input.extend_from_slice(idempotency_key.as_bytes());
    input.extend_from_slice(&retry_number.to_le_bytes());
    let digest = blake3::hash(&input);
    let sample = u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("digest slice"));
    let cap_millis = cap.as_millis() as u64;
    Duration::from_millis(sample % (cap_millis + 1))
}

fn retry_exhausted(error: LoopError, attempts: usize, elapsed: Duration) -> LoopError {
    match error {
        LoopError::Provider(message) => LoopError::Provider(crate::fatal::append_provider_context(
            message,
            &format!(
                " after {attempts} attempts over {} seconds",
                elapsed.as_secs()
            ),
        )),
        other => other,
    }
}

async fn retry_failure(
    error: LoopError,
    delay: RetryDelay,
    retries: &mut usize,
    started: tokio::time::Instant,
    deadline: &mut tokio::time::Instant,
    idempotency_key: &str,
    cancellation: Option<agentkit_core::TurnCancellation>,
) -> Result<(), LoopError> {
    let now = tokio::time::Instant::now();
    if let RetryDelay::RateLimitReset(reset) = delay {
        *deadline = (now + reset + RATE_LIMIT_RESET_GRACE).min(started + RETRY_BUDGET);
    }
    let attempts = retries.saturating_add(1);
    if now >= *deadline {
        return Err(retry_exhausted(error, attempts, started.elapsed()));
    }
    let jitter = retry_backoff(idempotency_key, attempts);
    let wait = match delay {
        RetryDelay::Hint(hint) => jitter.max(hint.unwrap_or(Duration::ZERO).min(MAX_RETRY_HINT)),
        RetryDelay::RateLimitReset(reset) => jitter.max(reset).min(RATE_LIMIT_MAX_WAIT),
    };
    if now + wait >= *deadline {
        return Err(retry_exhausted(error, attempts, started.elapsed()));
    }
    *retries += 1;
    sleep_before_retry(Some(wait), cancellation).await
}

fn retriable_transport_error(stage: crate::fatal::TransportStage, error: &reqwest::Error) -> bool {
    let transport =
        error.is_timeout() || error.is_connect() || error.is_request() || error.is_body();
    match stage {
        crate::fatal::TransportStage::Request => !error.is_decode() && transport,
        // Response::bytes_stream wraps underlying body-frame failures as Decode.
        crate::fatal::TransportStage::Stream => transport || error.is_decode(),
    }
}

fn transport_error(
    stage: crate::fatal::TransportStage,
    error: &reqwest::Error,
    retryable: bool,
    attempt: usize,
    response_request_id: Option<&str>,
) -> LoopError {
    crate::fatal::provider_transport_error(stage, error, retryable, attempt, response_request_id)
}

async fn sleep_before_retry(
    delay: Option<Duration>,
    cancellation: Option<agentkit_core::TurnCancellation>,
) -> Result<(), LoopError> {
    let Some(delay) = delay else {
        return Ok(());
    };
    if let Some(cancel) = cancellation {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(LoopError::Cancelled),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    } else {
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

/// Bounded, printable excerpt of a failure response body, prefixed for message
/// concatenation; empty when the body is absent or unreadable.
async fn failure_body_excerpt(
    response: reqwest::Response,
    cancellation: Option<agentkit_core::TurnCancellation>,
    deadline: tokio::time::Instant,
) -> Result<String, LoopError> {
    const MAX_EXCERPT_BYTES: usize = 1024;
    let read = async move {
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(Ok(chunk)) = stream.next().await {
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_EXCERPT_BYTES {
                body.truncate(MAX_EXCERPT_BYTES);
                break;
            }
        }
        body
    };
    let timed = tokio::time::timeout_at(deadline, read);
    let body = if let Some(cancel) = cancellation {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LoopError::Cancelled),
            result = timed => result.unwrap_or_default(),
        }
    } else {
        timed.await.unwrap_or_default()
    };
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
    Ok(if excerpt.is_empty() {
        String::new()
    } else {
        format!(": {excerpt}")
    })
}

fn retriable_http_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status.as_u16(),
        408 | 425 | 429 | 500 | 502 | 503 | 504 | 529
    )
}

fn retriable_status_code(status: u64) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn set_provider_finish_reasons<'a>(
    metadata: &mut MetadataMap,
    reasons: impl IntoIterator<Item = &'a str>,
) {
    let reasons = reasons
        .into_iter()
        .filter(|reason| !reason.is_empty())
        .map(|reason| Value::String(reason.to_owned()))
        .collect::<Vec<_>>();
    if !reasons.is_empty() {
        metadata.insert(
            PROVIDER_FINISH_REASONS_METADATA.to_owned(),
            Value::Array(reasons),
        );
    }
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
mod usage_tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use agentkit_core::{
        CancellationController, DataRef, Delta, Item, ItemKind, MediaPart, MetadataMap, Modality,
        Part, PartKind, SessionId, ToolOutput, TurnId,
    };
    use agentkit_loop::{LoopError, ModelSession, ModelTurn, ModelTurnEvent};
    use bytes::Bytes;
    use futures_util::{StreamExt as _, stream};
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{
        CONTINUATION_METADATA, ContinuationContext, GENERATED_IMAGE_METADATA, MAX_RETRY_BACKOFF,
        MAX_RETRY_HINT, MAX_STREAM_BYTES, MAX_WIRE_BYTES, OpenAiSubscriptionSession,
        OpenAiSubscriptionTurn, OpenAiSubscriptionTurnInit, PROVIDER_FINISH_REASONS_METADATA,
        RATE_LIMIT_MAX_WAIT, RATE_LIMIT_RESET_GRACE, RETRY_BUDGET, ResponseFailure, RetryDelay,
        SubscriptionConfig, X_CODEX_TURN_STATE, classify_response_failure,
        classify_top_level_error, map_item, parse_context_windows, parse_rate_limit_reset,
        parse_usage, rate_limit_reset, request_body, retriable_http_status, retriable_status_code,
        retriable_transport_error, retry_backoff, retry_failure, set_provider_finish_reasons,
        sleep_before_retry, tool_output, zeroizing_extend,
    };

    #[test]
    fn request_serializes_selected_reasoning_effort_and_omits_default() {
        let request = agentkit_loop::TurnRequest {
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn"),
            transcript: Vec::new(),
            available_tools: Vec::new(),
            cache: None,
            metadata: MetadataMap::new(),
        };
        let binding = super::auth::CredentialBinding {
            account_id: "account".into(),
            generation: "generation".into(),
        };

        let default = request_body("gpt-5.4", None, &request, &binding, "session").unwrap();
        let high = request_body(
            "gpt-5.4",
            Some(super::super::adapter::ReasoningEffort::High),
            &request,
            &binding,
            "session",
        )
        .unwrap();

        assert_eq!(default["reasoning"], json!({"summary": "auto"}));
        assert_eq!(
            high["reasoning"],
            json!({"summary": "auto", "effort": "high"})
        );
    }

    #[test]
    fn subscription_session_reports_initial_provider_identity() {
        let session = OpenAiSubscriptionSession {
            config: SubscriptionConfig::new("gpt-5.4".into()).unwrap(),
            reasoning_effort: None,
            response_attempt_replacement: false,
            client: reqwest::Client::new(),
            session_id: "provider-identity-test".into(),
            binding: super::auth::CredentialBinding {
                account_id: "test-account".into(),
                generation: "test-generation".into(),
            },
            context_windows: Arc::new(HashMap::new()),
            test_credentials: None,
        };

        assert_eq!(session.provider_name(), Some("openai-subscription"));
    }

    #[test]
    fn provider_finish_reasons_filter_empty_values() {
        let mut metadata = Default::default();

        set_provider_finish_reasons(&mut metadata, ["", "completed", ""]);

        assert_eq!(
            metadata[PROVIDER_FINISH_REASONS_METADATA],
            json!(["completed"])
        );
    }

    #[test]
    fn completed_and_incomplete_responses_keep_native_finish_reasons() {
        let mut completed = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        completed
            .consume_value(
                "response.created",
                &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
            )
            .unwrap();
        completed
            .consume_value(
                "response.completed",
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_test_123"}
                }),
            )
            .unwrap();
        let completed = completed
            .queued
            .iter()
            .find_map(|event| match event {
                ModelTurnEvent::Finished(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            completed.metadata[PROVIDER_FINISH_REASONS_METADATA],
            json!(["completed"])
        );

        for (reason, expected) in [
            ("max_output_tokens", agentkit_core::FinishReason::MaxTokens),
            ("content_filter", agentkit_core::FinishReason::Blocked),
        ] {
            let mut incomplete =
                OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
            incomplete
                .consume_value(
                    "response.created",
                    &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
                )
                .unwrap();
            incomplete
                .consume_value(
                    "response.incomplete",
                    &json!({
                        "type": "response.incomplete",
                        "response": {
                            "id": "resp_test_123",
                            "incomplete_details": {"reason": reason}
                        }
                    }),
                )
                .unwrap();
            let incomplete = incomplete
                .queued
                .iter()
                .find_map(|event| match event {
                    ModelTurnEvent::Finished(result) => Some(result),
                    _ => None,
                })
                .unwrap();
            assert_eq!(incomplete.finish_reason, expected);
            assert_eq!(
                incomplete.metadata[PROVIDER_FINISH_REASONS_METADATA],
                json!([reason])
            );
        }
    }

    #[test]
    fn responses_keepalive_and_unknown_events_are_ignored() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);

        turn.consume_frame(
            br#"event: keepalive
data: {"type":"keepalive","sequence_number":0}
"#,
        )
        .unwrap();
        turn.consume_frame(
            br#"event: response.created
data: {"type":"response.created","sequence_number":1,"response":{"id":"resp_test_123"}}
"#,
        )
        .unwrap();

        assert!(turn.created);
        turn.consume_frame(
            br#"event: future.event
data: {"type":"future.event","sequence_number":2}
"#,
        )
        .unwrap();
        turn.consume_frame(
            br#"event: response.future.delta
data: {"type":"response.future.delta","sequence_number":3,"delta":"ignored"}
"#,
        )
        .unwrap();
    }

    #[test]
    fn response_metadata_captures_turn_state_and_effective_model() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.metadata",
            &json!({
                "type": "response.metadata",
                "headers": {
                    "X-Codex-Turn-State": "sticky-state",
                    "X-OpenAI-Model": ["gpt-5.4-mini"]
                }
            }),
        )
        .unwrap();

        assert_eq!(turn.turn_state.as_deref(), Some("sticky-state"));
        assert!(!turn.turn_state_from_header);
        assert_eq!(turn.response_model.as_deref(), Some("gpt-5.4-mini"));
    }

    #[test]
    fn response_metadata_turn_state_must_match_existing_identity() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.turn_state = Some("http-state".to_owned());
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.metadata",
            &json!({
                "type": "response.metadata",
                "headers": {"X-Codex-Turn-State": "http-state"}
            }),
        )
        .unwrap();

        let error = turn
            .consume_value(
                "response.metadata",
                &json!({
                    "type": "response.metadata",
                    "response": {
                        "headers": {"x-codex-turn-state": "http-state"}
                    },
                    "headers": {"X-Codex-Turn-State": "different-state"}
                }),
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("response metadata changed x-codex-turn-state"),
            "{error}"
        );
    }

    #[test]
    fn request_maps_inline_image_and_audio_to_codex_content_items() {
        let item = Item::new(
            ItemKind::User,
            vec![
                Part::Media(MediaPart::new(
                    Modality::Image,
                    "image/png",
                    DataRef::InlineBytes(vec![1, 2, 3]),
                )),
                Part::Media(MediaPart::new(
                    Modality::Audio,
                    "audio/wav",
                    DataRef::InlineText("data:audio/wav;base64,BAUG".to_owned()),
                )),
            ],
        );

        let mapped = map_item(&item, None).unwrap();

        assert_eq!(
            mapped,
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_image",
                        "image_url": "data:image/png;base64,AQID",
                        "detail": "high"
                    },
                    {
                        "type": "input_audio",
                        "audio_url": "data:audio/wav;base64,BAUG"
                    }
                ]
            })]
        );
    }

    #[test]
    fn tool_output_maps_media_to_structured_codex_content() {
        let output = ToolOutput::Parts(vec![
            Part::text("waveform"),
            Part::Media(MediaPart::new(
                Modality::Audio,
                "audio/wav",
                DataRef::InlineBytes(vec![1, 2, 3]),
            )),
        ]);

        assert_eq!(
            tool_output(&output).unwrap(),
            json!([
                {"type": "input_text", "text": "waveform"},
                {
                    "type": "input_audio",
                    "audio_url": "data:audio/wav;base64,AQID"
                }
            ])
        );
    }

    #[test]
    fn request_rejects_video_and_unreadable_media_uris_without_exposing_data() {
        let video = Item::new(
            ItemKind::User,
            vec![Part::Media(MediaPart::new(
                Modality::Video,
                "video/mp4",
                DataRef::InlineText("sensitive-base64".to_owned()),
            ))],
        );
        let video_error = map_item(&video, None).unwrap_err().to_string();
        assert!(video_error.contains("does not support video"));
        assert!(!video_error.contains("sensitive-base64"));

        let local = Item::new(
            ItemKind::User,
            vec![Part::Media(MediaPart::new(
                Modality::Image,
                "image/png",
                DataRef::Uri("file:///secret/image.png".to_owned()),
            ))],
        );
        let uri_error = map_item(&local, None).unwrap_err().to_string();
        assert!(uri_error.contains("provide inline bytes"));
        assert!(!uri_error.contains("/secret/image.png"));
    }

    #[test]
    fn generated_image_streams_placeholder_persists_media_and_replays() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "image-1"}
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "image-1",
                    "type": "image_generation_call",
                    "status": "completed",
                    "revised_prompt": "a blue square",
                    "result": "AQID"
                }
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.completed",
            &json!({
                "type": "response.completed",
                "response": {"id": "resp_test_123"}
            }),
        )
        .unwrap();

        assert!(matches!(
            turn.queued.front(),
            Some(ModelTurnEvent::Delta(Delta::BeginPart {
                kind: PartKind::Text,
                ..
            }))
        ));
        assert!(matches!(
            turn.queued.get(1),
            Some(ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }))
                if chunk == "[Image #1]" && !chunk.contains("AQID")
        ));
        let output = turn
            .queued
            .iter()
            .find_map(|event| match event {
                ModelTurnEvent::Finished(result) => Some(&result.output_items),
                _ => None,
            })
            .expect("expected finished output");
        let Part::Media(media) = &output[0].parts[0] else {
            panic!("expected persisted generated image media");
        };
        assert_eq!(media.mime_type, "image/png");
        assert_eq!(media.data, DataRef::InlineBytes(vec![1, 2, 3]));
        assert_eq!(
            media.metadata[GENERATED_IMAGE_METADATA]["revised_prompt"],
            json!("a blue square")
        );
        assert_eq!(
            media.metadata[CONTINUATION_METADATA]["response_id"],
            json!("resp_test_123")
        );

        let binding = super::auth::CredentialBinding {
            account_id: "test-account".to_owned(),
            generation: "test-generation".to_owned(),
        };
        let continuation = ContinuationContext {
            model: "gpt-5.4",
            binding: &binding,
            session_id: "s",
        };
        assert_eq!(
            map_item(&output[0], Some(&continuation)).unwrap(),
            vec![json!({
                "id": "image-1",
                "type": "image_generation_call",
                "status": "completed",
                "revised_prompt": "a blue square",
                "result": "AQID"
            })]
        );

        let other_binding = super::auth::CredentialBinding {
            account_id: "other-account".to_owned(),
            generation: "test-generation".to_owned(),
        };
        let mismatched = ContinuationContext {
            model: "gpt-5.4",
            binding: &other_binding,
            session_id: "s",
        };
        assert!(map_item(&output[0], Some(&mismatched)).is_err());
    }

    #[test]
    fn empty_response_text_is_accepted_like_official_codex() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "message-1"}
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.output_text.delta",
            &json!({
                "type": "response.output_text.delta",
                "item_id": "message-1",
                "output_index": 0,
                "content_index": 0,
                "delta": ""
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "message-1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": ""}]
                }
            }),
        )
        .unwrap();

        let Part::Text(text) = &turn.output[0].parts[0] else {
            panic!("expected empty text output");
        };
        assert_eq!(text.text, "");
    }

    #[test]
    fn empty_reasoning_delta_is_accepted_like_official_codex() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "reasoning-1"}
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.reasoning_summary_text.delta",
            &json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": "reasoning-1",
                "output_index": 0,
                "summary_index": 0,
                "delta": ""
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "reasoning-1",
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": ""}],
                    "encrypted_content": "ciphertext"
                }
            }),
        )
        .unwrap();
    }

    #[test]
    fn reasoning_summaries_stream_and_are_stored() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "reasoning-1"}
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.reasoning_summary_text.delta",
            &json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": "reasoning-1",
                "output_index": 0,
                "summary_index": 0,
                "delta": "Inspecting the repository"
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "reasoning-1",
                    "type": "reasoning",
                    "summary": [{
                        "type": "summary_text",
                        "text": "Inspecting the repository"
                    }],
                    "encrypted_content": "ciphertext"
                }
            }),
        )
        .unwrap();

        assert!(matches!(
            &turn.queued[0],
            ModelTurnEvent::Delta(Delta::BeginPart {
                kind: PartKind::Reasoning,
                ..
            })
        ));
        assert!(matches!(
            &turn.queued[1],
            ModelTurnEvent::Delta(Delta::AppendText { chunk, .. })
                if chunk == "Inspecting the repository"
        ));
        assert!(matches!(
            &turn.queued[2],
            ModelTurnEvent::Delta(Delta::CommitPart {
                part: Part::Reasoning(reasoning)
            }) if reasoning.summary.as_deref() == Some("Inspecting the repository")
        ));
        let Part::Reasoning(reasoning) = &turn.output[0].parts[0] else {
            panic!("expected stored reasoning");
        };
        assert_eq!(
            reasoning.summary.as_deref(),
            Some("Inspecting the repository")
        );
        assert_eq!(
            reasoning.metadata[CONTINUATION_METADATA]["encrypted_content"],
            json!("ciphertext")
        );

        turn.consume_value(
            "response.completed",
            &json!({
                "type": "response.completed",
                "response": {"id": "resp_test_123", "model": "gpt-5.4"}
            }),
        )
        .unwrap();
        let finished = turn.queued.iter().find_map(|event| match event {
            ModelTurnEvent::Finished(result) => Some(result),
            _ => None,
        });
        let Part::Reasoning(reasoning) = &finished.unwrap().output_items[0].parts[0] else {
            panic!("expected finished reasoning");
        };
        assert_eq!(
            reasoning.metadata[CONTINUATION_METADATA]["response_id"],
            json!("resp_test_123")
        );
    }

    #[test]
    fn multiple_reasoning_summaries_keep_wire_and_stored_separators() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "reasoning-1"}
            }),
        )
        .unwrap();
        for (index, delta) in [(0, "first"), (1, "second")] {
            turn.consume_value(
                "response.reasoning_summary_text.delta",
                &json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": "reasoning-1",
                    "output_index": 0,
                    "summary_index": index,
                    "delta": delta
                }),
            )
            .unwrap();
        }
        turn.consume_value(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "reasoning-1",
                    "type": "reasoning",
                    "summary": [
                        {"type": "summary_text", "text": "first"},
                        {"type": "summary_text", "text": "second"}
                    ],
                    "encrypted_content": "ciphertext"
                }
            }),
        )
        .unwrap();

        let streamed = turn
            .queued
            .iter()
            .filter_map(|event| match event {
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => Some(chunk.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(streamed, "first\n\nsecond");
        let committed = turn
            .queued
            .iter()
            .filter_map(|event| match event {
                ModelTurnEvent::Delta(Delta::CommitPart {
                    part: Part::Reasoning(reasoning),
                }) => reasoning.summary.as_deref(),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(committed, streamed);
        let Part::Reasoning(reasoning) = &turn.output[0].parts[0] else {
            panic!("expected stored reasoning");
        };
        assert_eq!(reasoning.summary.as_deref(), Some(streamed.as_str()));
    }

    #[test]
    fn reasoning_summary_must_match_streamed_deltas() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_test_123"}}),
        )
        .unwrap();
        turn.consume_value(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "reasoning-1"}
            }),
        )
        .unwrap();
        turn.consume_value(
            "response.reasoning_summary_text.delta",
            &json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": "reasoning-1",
                "output_index": 0,
                "summary_index": 0,
                "delta": "first"
            }),
        )
        .unwrap();
        let error = turn
            .consume_value(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": "reasoning-1",
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": "different"}],
                        "encrypted_content": "ciphertext"
                    }
                }),
            )
            .unwrap_err()
            .to_string();

        assert!(error.contains("differs from streamed summary deltas"));
    }

    #[test]
    fn retry_status_policy_is_conservative() {
        for status in [408_u16, 425, 429, 500, 502, 503, 504, 529] {
            let status = reqwest::StatusCode::from_u16(status).unwrap();
            assert!(retriable_http_status(status), "{status}");
            assert!(retriable_status_code(u64::from(status.as_u16())));
        }
        for status in [400_u16, 401, 402, 403, 409, 422, 501, 505] {
            let status = reqwest::StatusCode::from_u16(status).unwrap();
            assert!(!retriable_http_status(status), "{status}");
            assert!(!retriable_status_code(u64::from(status.as_u16())));
        }
    }

    #[test]
    fn retry_backoff_is_deterministic_and_bounded() {
        for retry in 1..=40 {
            let delay = retry_backoff("stable-idempotency-key", retry);
            let exponent = retry.saturating_sub(1).min(5) as u32;
            let cap = Duration::from_secs(1_u64 << exponent).min(MAX_RETRY_BACKOFF);
            assert!(delay <= cap, "retry {retry}: {delay:?} > {cap:?}");
            assert_eq!(
                delay,
                retry_backoff("stable-idempotency-key", retry),
                "retry jitter must remain stable across a replay"
            );
        }
    }

    #[tokio::test]
    async fn retry_budget_stops_at_the_deadline() {
        let started = tokio::time::Instant::now();
        let mut deadline = started;
        let mut retries = 25;
        let error = retry_failure(
            LoopError::Provider("transient".into()),
            RetryDelay::Hint(None),
            &mut retries,
            started,
            &mut deadline,
            "key",
            None,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("after 26 attempts"), "{error}");
        assert_eq!(retries, 25);
    }

    #[tokio::test]
    async fn rate_limit_reset_extends_the_deadline_and_caps_the_wait() {
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let mut deadline = started;
        let mut retries = 0;
        let reset = Duration::from_secs(3600);
        retry_failure(
            LoopError::Provider("rate limited".into()),
            RetryDelay::RateLimitReset(reset),
            &mut retries,
            started,
            &mut deadline,
            "key",
            None,
        )
        .await
        .unwrap();

        assert_eq!(deadline, started + reset + RATE_LIMIT_RESET_GRACE);
        assert_eq!(retries, 1);
        let waited = started.elapsed();
        assert!(
            waited >= RATE_LIMIT_MAX_WAIT && waited < RATE_LIMIT_MAX_WAIT + Duration::from_secs(1),
            "{waited:?}"
        );
    }

    #[tokio::test]
    async fn rate_limit_reset_never_extends_past_the_retry_budget() {
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let mut deadline = started + RETRY_BUDGET;
        let mut retries = 0;
        retry_failure(
            LoopError::Provider("rate limited".into()),
            RetryDelay::RateLimitReset(RETRY_BUDGET * 2),
            &mut retries,
            started,
            &mut deadline,
            "key",
            None,
        )
        .await
        .unwrap();

        assert_eq!(deadline, started + RETRY_BUDGET);
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "client closed before request headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            assert!(request.len() <= 64 * 1024, "request headers are too large");
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
        while request.len() < header_end + content_length {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "client closed before request body");
            request.extend_from_slice(&buffer[..read]);
        }
        request
    }

    fn request_header(request: &[u8], expected: &str) -> String {
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        std::str::from_utf8(&request[..header_end])
            .unwrap()
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(expected)
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("missing {expected} header"))
    }

    #[tokio::test]
    async fn begin_turn_resends_after_an_early_stream_failure() {
        const SUCCESS: &[u8] = br#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_test_123"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"item-1"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":2,"item_id":"item-1","output_index":0,"content_index":0,"delta":"hello"}

"#;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut idempotency_keys = Vec::new();
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                idempotency_keys.push(request_header(&request, "idempotency-key"));
                if attempt == 0 {
                    let body = b": ignored first attempt\n\n";
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len() + 1024
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    socket.write_all(body).await.unwrap();
                } else {
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                SUCCESS.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    socket.write_all(SUCCESS).await.unwrap();
                }
                socket.shutdown().await.unwrap();
            }
            idempotency_keys
        });

        let credentials = super::auth::TokenRecord::for_test("access", "test-account");
        let binding = credentials.binding().unwrap();
        let mut session = OpenAiSubscriptionSession {
            config: SubscriptionConfig::new("gpt-5.4".into())
                .unwrap()
                .with_endpoint(format!("http://{address}/responses")),
            reasoning_effort: None,
            response_attempt_replacement: false,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            session_id: "stream-retry-test".into(),
            binding,
            context_windows: Arc::new(HashMap::new()),
            test_credentials: Some(credentials),
        };
        let request = agentkit_loop::TurnRequest {
            session_id: SessionId::new("stream-retry-test"),
            turn_id: TurnId::new("turn-1"),
            transcript: vec![Item::text(ItemKind::User, "hello")],
            available_tools: Vec::new(),
            cache: None,
            metadata: MetadataMap::new(),
        };

        let _turn = session.begin_turn(request, None).await.unwrap();
        let idempotency_keys = server.await.unwrap();
        assert_eq!(idempotency_keys.len(), 2);
        assert!(!idempotency_keys[0].is_empty());
        assert_eq!(idempotency_keys[0], idempotency_keys[1]);
    }

    fn push_sse(body: &mut Vec<u8>, kind: &str, value: serde_json::Value) {
        body.extend_from_slice(format!("event: {kind}\ndata: {value}\n\n").as_bytes());
    }

    fn text_attempt(response_id: &str, item_id: &str, text: &str, completed: bool) -> Vec<u8> {
        let mut body = Vec::new();
        push_sse(
            &mut body,
            "response.created",
            json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": {"id": response_id}
            }),
        );
        push_sse(
            &mut body,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "sequence_number": 1,
                "output_index": 0,
                "item": {"id": item_id}
            }),
        );
        push_sse(
            &mut body,
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 2,
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "delta": text
            }),
        );
        if completed {
            push_sse(
                &mut body,
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": 3,
                    "output_index": 0,
                    "item": {
                        "id": item_id,
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]
                    }
                }),
            );
            push_sse(
                &mut body,
                "response.completed",
                json!({
                    "type": "response.completed",
                    "sequence_number": 4,
                    "response": {"id": response_id, "model": "gpt-5.4"}
                }),
            );
        }
        body
    }

    fn failed_text_and_tool_attempt() -> Vec<u8> {
        let mut body = text_attempt("resp_failed", "text-failed", "discard me", false);
        push_sse(
            &mut body,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "sequence_number": 3,
                "output_index": 1,
                "item": {"id": "tool-failed"}
            }),
        );
        push_sse(
            &mut body,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "sequence_number": 4,
                "output_index": 1,
                "item": {
                    "id": "tool-failed",
                    "type": "function_call",
                    "call_id": "call-failed",
                    "name": "dangerous_tool",
                    "arguments": "{}"
                }
            }),
        );
        body
    }

    async fn write_sse_response(
        socket: &mut tokio::net::TcpStream,
        body: &[u8],
        declared_length: usize,
    ) {
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(body).await.unwrap();
        socket.shutdown().await.unwrap();
    }

    fn replacement_test_session(
        address: std::net::SocketAddr,
        enabled: bool,
    ) -> OpenAiSubscriptionSession {
        let credentials = super::auth::TokenRecord::for_test("access", "test-account");
        let binding = credentials.binding().unwrap();
        OpenAiSubscriptionSession {
            config: SubscriptionConfig::new("gpt-5.4".into())
                .unwrap()
                .with_endpoint(format!("http://{address}/responses")),
            reasoning_effort: None,
            response_attempt_replacement: enabled,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            session_id: "response-replacement-test".into(),
            binding,
            context_windows: Arc::new(HashMap::new()),
            test_credentials: Some(credentials),
        }
    }

    fn replacement_test_request() -> agentkit_loop::TurnRequest {
        agentkit_loop::TurnRequest {
            session_id: SessionId::new("response-replacement-test"),
            turn_id: TurnId::new("turn-1"),
            transcript: vec![Item::text(ItemKind::User, "hello")],
            available_tools: Vec::new(),
            cache: None,
            metadata: MetadataMap::new(),
        }
    }

    #[tokio::test]
    async fn nondeterministic_response_replacement_is_authoritative_and_ordered() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let failed = failed_text_and_tool_attempt();
        let replacement = text_attempt("resp_replacement", "text-replacement", "keep me", true);
        let server = tokio::spawn(async move {
            let mut keys = Vec::new();
            for (attempt, body) in [failed, replacement].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                keys.push(request_header(&request, "idempotency-key"));
                let declared = body.len() + if attempt == 0 { 1_024 } else { 0 };
                write_sse_response(&mut socket, &body, declared).await;
            }
            keys
        });

        let mut session = replacement_test_session(address, true);
        let mut turn = session
            .begin_turn(replacement_test_request(), None)
            .await
            .unwrap();
        let mut observed = Vec::new();
        let mut finished = None;
        while let Some(event) = turn.next_event(None).await.unwrap() {
            match &event {
                ModelTurnEvent::ToolCall(call) => {
                    panic!(
                        "tool call escaped before replacement completed: {}",
                        call.name
                    )
                }
                ModelTurnEvent::Finished(result) => finished = Some(result.clone()),
                _ => {}
            }
            observed.push(event);
        }

        let markers = observed
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                ModelTurnEvent::Delta(delta) if crate::response_attempt::is_marker(delta) => {
                    Some(index)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(markers.len(), 1);
        let chunks = observed
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => {
                    Some((index, chunk.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chunks.iter().map(|(_, text)| *text).collect::<Vec<_>>(),
            ["discard me", "keep me"]
        );
        assert!(chunks[0].0 < markers[0] && markers[0] < chunks[1].0);

        let finished = finished.expect("replacement must finish");
        let final_text = finished
            .output_items
            .iter()
            .flat_map(|item| &item.parts)
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(final_text, "keep me");
        assert!(!final_text.contains("discard me"));
        assert_eq!(finished.response_id.as_deref(), Some("resp_replacement"));

        let keys = server.await.unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1]);
    }

    #[tokio::test]
    async fn repeated_response_replacement_emits_one_marker_per_escaped_attempt() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = [
            text_attempt("resp_one", "item-one", "one", false),
            text_attempt("resp_two", "item-two", "two", false),
            text_attempt("resp_three", "item-three", "three", true),
        ];
        let server = tokio::spawn(async move {
            for (index, body) in attempts.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _request = read_http_request(&mut socket).await;
                let declared = body.len() + if index < 2 { 1_024 } else { 0 };
                write_sse_response(&mut socket, &body, declared).await;
            }
        });

        let mut session = replacement_test_session(address, true);
        let mut turn = session
            .begin_turn(replacement_test_request(), None)
            .await
            .unwrap();
        let mut order = Vec::new();
        while let Some(event) = turn.next_event(None).await.unwrap() {
            match event {
                ModelTurnEvent::Delta(ref delta) if crate::response_attempt::is_marker(delta) => {
                    order.push("marker".to_owned());
                }
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => order.push(chunk),
                _ => {}
            }
        }
        assert_eq!(order, ["one", "marker", "two", "marker", "three"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn outputless_replacement_failures_keep_retrying_without_extra_markers() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = [
            text_attempt("resp_visible", "item-visible", "discard", false),
            Vec::new(),
            Vec::new(),
            text_attempt("resp_final", "item-final", "keep", true),
        ];
        let server = tokio::spawn(async move {
            for (index, body) in attempts.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _request = read_http_request(&mut socket).await;
                let declared = body.len() + if index < 3 { 1_024 } else { 0 };
                write_sse_response(&mut socket, &body, declared).await;
            }
        });

        let mut session = replacement_test_session(address, true);
        let mut turn = session
            .begin_turn(replacement_test_request(), None)
            .await
            .unwrap();
        let mut order = Vec::new();
        while let Some(event) = turn.next_event(None).await.unwrap() {
            match event {
                ModelTurnEvent::Delta(ref delta) if crate::response_attempt::is_marker(delta) => {
                    order.push("marker".to_owned());
                }
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => order.push(chunk),
                _ => {}
            }
        }
        assert_eq!(order, ["discard", "marker", "keep"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_turn_state_is_sent_and_retained_across_retries() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mut metadata_only = Vec::new();
        push_sse(
            &mut metadata_only,
            "response.created",
            json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": {"id": "resp_metadata"}
            }),
        );
        push_sse(
            &mut metadata_only,
            "response.metadata",
            json!({
                "type": "response.metadata",
                "sequence_number": 1,
                "headers": {"x-codex-turn-state": "metadata-state"}
            }),
        );
        let attempts = [
            metadata_only,
            Vec::new(),
            text_attempt("resp_final", "item-final", "done", true),
        ];
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (index, body) in attempts.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_http_request(&mut socket).await);
                let declared = body.len() + if index < 2 { 1_024 } else { 0 };
                write_sse_response(&mut socket, &body, declared).await;
            }
            requests
        });

        let mut session = replacement_test_session(address, true);
        let mut turn = session
            .begin_turn(replacement_test_request(), None)
            .await
            .unwrap();
        while turn.next_event(None).await.unwrap().is_some() {}

        let requests = server.await.unwrap();
        assert_eq!(
            request_header(&requests[1], X_CODEX_TURN_STATE),
            "metadata-state"
        );
        assert_eq!(
            request_header(&requests[2], X_CODEX_TURN_STATE),
            "metadata-state"
        );
    }

    #[tokio::test]
    async fn post_event_failure_is_fatal_without_replacement_capability() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let failed = text_attempt("resp_failed", "item-failed", "visible", false);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _request = read_http_request(&mut socket).await;
            let declared = failed.len() + 1_024;
            write_sse_response(&mut socket, &failed, declared).await;
        });

        let mut session = replacement_test_session(address, false);
        let mut turn = session
            .begin_turn(replacement_test_request(), None)
            .await
            .unwrap();
        assert!(turn.request_context.is_none());
        let mut saw_text = false;
        let error = loop {
            match turn.next_event(None).await {
                Ok(Some(ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }))) => {
                    saw_text |= chunk == "visible";
                }
                Ok(Some(ModelTurnEvent::Delta(ref delta))) => {
                    assert!(!crate::response_attempt::is_marker(delta));
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("truncated attempt unexpectedly completed"),
                Err(error) => break error,
            }
        };
        assert!(saw_text);
        assert!(matches!(error, LoopError::Provider(_)));
        server.await.unwrap();
    }

    async fn truncated_reqwest_response(body: &'static [u8]) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 512];
            while request.len() < 8 * 1024 {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let declared = body.len() + 1_024;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nX-Request-Id: req_truncated-1\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(body).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/responses"))
            .send()
            .await
            .unwrap();
        server.await.unwrap();
        response
    }

    async fn turn_from_truncated_reqwest_response(body: &'static [u8]) -> OpenAiSubscriptionTurn {
        let response = truncated_reqwest_response(body).await;
        let request_id = crate::fatal::safe_response_request_id(response.headers());
        OpenAiSubscriptionTurn::new_inner(
            response.bytes_stream(),
            OpenAiSubscriptionTurnInit {
                requested_model: "gpt-5.4".into(),
                header_model: None,
                turn_state: None,
                turn_state_from_header: false,
                binding: super::auth::CredentialBinding {
                    account_id: "test-account".to_owned(),
                    generation: "test-generation".to_owned(),
                },
                session_id: "s".to_owned(),
                context_windows: Arc::new(HashMap::new()),
                response_attempt_replacement: false,
                attempt: 1,
                response_request_id: request_id,
                wire_bytes: 0,
                attempt_wire_bytes: 0,
                request_context: None,
            },
        )
    }

    #[tokio::test]
    async fn truncated_reqwest_body_is_retryable_before_and_after_a_model_event() {
        let mut stream = truncated_reqwest_response(b"incomplete")
            .await
            .bytes_stream();
        let wrapped = loop {
            match stream.next().await {
                Some(Ok(_)) => continue,
                Some(Err(error)) => break error,
                None => panic!("truncated response ended without reqwest error"),
            }
        };
        assert!(wrapped.is_decode());
        assert!(!retriable_transport_error(
            crate::fatal::TransportStage::Request,
            &wrapped
        ));
        assert!(retriable_transport_error(
            crate::fatal::TransportStage::Stream,
            &wrapped
        ));

        let mut early =
            turn_from_truncated_reqwest_response(b"event: response.created\ndata: {\"type\":")
                .await;
        let error = early.next_event(None).await.unwrap_err().to_string();
        let diagnostics =
            crate::fatal::transport_diagnostics_json(&error).expect("transport marker");
        assert!(early.retryable_transport_failure);
        assert_eq!(diagnostics["stage"], "stream");
        assert_eq!(diagnostics["retryable"], true);
        assert_eq!(diagnostics["attempt"], 1);
        assert_eq!(diagnostics["response_request_id"], "req_truncated-1");
        assert_eq!(diagnostics["reqwest"]["decode"], true);
        let sources = diagnostics["source_chain"].as_array().unwrap();
        assert!(sources.iter().any(|source| source["kind"] == "hyper"));
        assert!(
            sources.iter().any(
                |source| source["kind"] == "io" && source["classification"] == "unexpected_eof"
            ),
            "{diagnostics}"
        );

        let mut late = turn_from_truncated_reqwest_response(
            br#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_test_123"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"item-1"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":2,"item_id":"item-1","output_index":0,"content_index":0,"delta":"hello"}

event: response.created
data: {"# ,
        )
        .await;
        assert!(late.next_event(None).await.unwrap().is_some());
        let error = loop {
            match late.next_event(None).await {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("truncated response completed"),
                Err(error) => break error.to_string(),
            }
        };
        let diagnostics =
            crate::fatal::transport_diagnostics_json(&error).expect("transport marker");
        assert!(late.retryable_transport_failure);
        assert_eq!(diagnostics["retryable"], true);
        assert_eq!(diagnostics["reqwest"]["decode"], true);
    }

    #[tokio::test]
    async fn stream_close_before_first_event_is_retryable() {
        let mut turn = OpenAiSubscriptionTurn::new(
            stream::empty::<Result<Bytes, reqwest::Error>>(),
            "gpt-5.4".into(),
            None,
        );

        assert!(turn.next_event(None).await.is_err());
        assert!(turn.retryable_transport_failure);
    }

    #[test]
    fn top_level_errors_are_classified_without_exposing_provider_messages() {
        let failure = classify_top_level_error(&json!({
            "type": "error",
            "code": "server_error",
            "message": "sensitive provider detail",
            "retry_after": "9999"
        }));

        assert!(failure.retriable);
        assert_eq!(failure.retry_after, Some(MAX_RETRY_HINT));
        assert_eq!(
            failure.message,
            "openai-subscription transient response failed: error/server_error"
        );
        assert!(!failure.message.contains("sensitive"));

        let permanent = classify_top_level_error(&json!({
            "type": "error",
            "code": "invalid_request_error",
            "message": "do not expose me"
        }));
        assert!(!permanent.retriable);
        assert_eq!(permanent.retry_after, None);
    }

    #[test]
    fn response_failed_uses_the_same_retriable_classification() {
        let failure = classify_response_failure(&json!({
            "type": "response.failed",
            "response": {
                "status_code": 503,
                "retry_after": 2,
                "error": {"type": "server_error", "code": "internal_error"}
            }
        }));

        assert!(failure.retriable);
        assert_eq!(failure.retry_after, Some(Duration::from_secs(2)));

        for (status, code) in [
            (409, "server_error"),
            (501, "server_error"),
            (505, "internal_error"),
            (429, "insufficient_quota"),
            (503, "billing_hard_limit_reached"),
        ] {
            let failure = classify_response_failure(&json!({
                "type": "response.failed",
                "response": {
                    "status_code": status,
                    "error": {"type": "server_error", "code": code}
                }
            }));
            assert!(!failure.retriable, "status={status} code={code}");
        }
    }

    #[test]
    fn statusless_unknown_codes_default_to_retriable() {
        for (error_type, code) in [
            ("service_unavailable_error", "server_is_overloaded"),
            ("brand_new_error", "never_seen_before"),
        ] {
            let failure = classify_response_failure(&json!({
                "type": "response.failed",
                "response": {"error": {"type": error_type, "code": code}}
            }));
            assert!(failure.retriable, "{error_type}/{code}");
        }

        for (error_type, code) in [
            ("invalid_request_error", "invalid_prompt"),
            ("quota_error", "quota_exceeded"),
            ("request_error", "model_not_found"),
            ("authentication_error", "expired"),
        ] {
            let failure = classify_response_failure(&json!({
                "type": "response.failed",
                "response": {"error": {"type": error_type, "code": code}}
            }));
            assert!(!failure.retriable, "{error_type}/{code}");
        }
    }

    #[test]
    fn rate_limit_reset_headers_parse_and_win_over_retry_after() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(rate_limit_reset(&headers), None);

        headers.insert("x-ratelimit-reset-requests", "1.5".parse().unwrap());
        headers.insert("x-ratelimit-reset-tokens", "6m30s".parse().unwrap());
        assert_eq!(rate_limit_reset(&headers), Some(Duration::from_secs(390)));

        assert_eq!(
            parse_rate_limit_reset("250ms"),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            parse_rate_limit_reset("1h2m3s"),
            Some(Duration::from_secs(3723))
        );
        assert_eq!(parse_rate_limit_reset("soon"), None);
        assert_eq!(parse_rate_limit_reset("-5"), None);
        assert_eq!(parse_rate_limit_reset(""), None);
        assert_eq!(parse_rate_limit_reset("0s"), Some(Duration::ZERO));
        assert_eq!(parse_rate_limit_reset("0"), Some(Duration::ZERO));
        let epoch_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 120;
        let reset = parse_rate_limit_reset(&epoch_seconds.to_string()).unwrap();
        assert!(reset > Duration::from_secs(110) && reset < Duration::from_secs(130));
    }

    #[tokio::test]
    async fn queued_output_precedes_a_terminal_top_level_error() {
        let chunk = Bytes::from_static(
            br#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_test_123"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"item-1"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":2,"item_id":"item-1","output_index":0,"content_index":0,"delta":"hello"}

event: error
data: {"type":"error","sequence_number":3,"code":"server_error","message":"retry"}

event: future.event
data: {"type":"future.event","sequence_number":4}

"#,
        );
        let mut turn = OpenAiSubscriptionTurn::new(
            stream::iter([Ok::<_, reqwest::Error>(chunk)]),
            "gpt-5.4".into(),
            None,
        );

        assert!(matches!(
            turn.next_event(None).await.unwrap(),
            Some(ModelTurnEvent::Delta(_))
        ));
        assert!(matches!(
            turn.next_event(None).await.unwrap(),
            Some(ModelTurnEvent::Delta(_))
        ));
        let error = turn.next_event(None).await.unwrap_err().to_string();
        assert!(error.contains("transient response failed: error/server_error"));
        assert!(!error.contains("unknown Responses SSE event"));
    }

    #[test]
    fn terminal_response_id_must_match_response_created() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        turn.consume_value(
            "response.created",
            &json!({
                "type": "response.created",
                "response": {"id": "resp_stable_123"}
            }),
        )
        .unwrap();
        let error = turn
            .consume_value(
                "response.completed",
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_changed_456"}
                }),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed the response.created ID"), "{error}");
        assert!(turn.request_context.is_none());
    }

    #[tokio::test]
    async fn terminal_response_rejects_partial_trailing_bytes_in_the_same_chunk() {
        for terminal in [
            r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_test_123"}}

"#,
            r#"event: response.incomplete
data: {"type":"response.incomplete","response":{"id":"resp_test_123","incomplete_details":{"reason":"max_output_tokens"}}}

"#,
        ] {
            let body = format!(
                "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_test_123\"}}}}\n\n{terminal}partial"
            );
            let mut turn = OpenAiSubscriptionTurn::new(
                stream::iter([Ok::<_, reqwest::Error>(Bytes::from(body))]),
                "gpt-5.4".into(),
                None,
            );

            let error = turn.next_event(None).await.unwrap_err().to_string();
            assert!(
                error.contains("terminal response was followed by trailing bytes"),
                "{error}"
            );
            assert!(turn.buffer.is_empty());
        }
    }

    #[test]
    fn zeroizing_extend_grows_geometrically_within_limit() {
        const CHUNK_BYTES: usize = 8 * 1024;
        const LIMIT: usize = 64 * 1024;

        let mut buffer = zeroize::Zeroizing::new(Vec::new());
        let mut previous_capacity = 0;
        let mut growths = 0;
        for _ in 0..LIMIT / CHUNK_BYTES {
            zeroizing_extend(&mut buffer, &[0x5a; CHUNK_BYTES], LIMIT, "test limit").unwrap();
            let capacity = buffer.capacity();
            assert!(capacity >= buffer.len());
            assert!(capacity <= LIMIT);
            if capacity != previous_capacity {
                growths += 1;
                if previous_capacity > 0 {
                    assert!(capacity >= (previous_capacity * 2).min(LIMIT));
                }
                previous_capacity = capacity;
            }
        }

        assert!(growths <= 4, "8 KiB chunks caused {growths} reallocations");
    }

    #[tokio::test]
    async fn wire_bytes_count_toward_aggregate_limit() {
        let mut within_limit = OpenAiSubscriptionTurn::new(
            stream::iter([Ok::<_, reqwest::Error>(Bytes::from_static(b"ab"))]),
            "gpt-5.4".into(),
            None,
        );
        within_limit.wire_bytes = MAX_WIRE_BYTES - 2;
        let error = within_limit.next_event(None).await.unwrap_err().to_string();
        assert!(
            error.contains("SSE stream closed before response.completed"),
            "{error}"
        );
        assert!(!error.contains("aggregate SSE wire ingress"), "{error}");

        let mut turn = OpenAiSubscriptionTurn::new(
            stream::iter([Ok::<_, reqwest::Error>(Bytes::from_static(b"ab"))]),
            "gpt-5.4".into(),
            None,
        );
        turn.wire_bytes = MAX_WIRE_BYTES - 1;
        let error = turn.next_event(None).await.unwrap_err().to_string();
        assert!(
            error.contains("aggregate SSE wire ingress exceeds 64 MiB"),
            "{error}"
        );

        let mut per_attempt = OpenAiSubscriptionTurn::new(
            stream::iter([Ok::<_, reqwest::Error>(Bytes::from_static(b"ab"))]),
            "gpt-5.4".into(),
            None,
        );
        per_attempt.wire_bytes = 123;
        per_attempt.attempt_wire_bytes = MAX_STREAM_BYTES - 1;
        let error = per_attempt.next_event(None).await.unwrap_err().to_string();
        assert!(
            error.contains("per-attempt SSE wire ingress exceeds 16 MiB"),
            "{error}"
        );

        per_attempt.attempt_wire_bytes = 456;
        per_attempt.reset_attempt_state();
        assert_eq!(per_attempt.attempt_wire_bytes, 0);
        assert_eq!(per_attempt.wire_bytes, 123);
    }

    #[tokio::test]
    async fn ready_cancellation_wins_over_zero_retry_sleep() {
        let controller = CancellationController::new();
        let cancellation = controller.handle().checkpoint();
        controller.interrupt();
        assert!(matches!(
            sleep_before_retry(Some(Duration::ZERO), Some(cancellation)).await,
            Err(LoopError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn required_replacement_marker_precedes_cancellation() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        let credentials = super::auth::TokenRecord::for_test("sensitive-access", "test-account");
        let binding = credentials.binding().unwrap();
        turn.request_context = Some(super::OpenAiSubscriptionRequestContext {
            config: SubscriptionConfig::new("gpt-5.4".into()).unwrap(),
            client: reqwest::Client::new(),
            binding,
            session_id: "sensitive-session".to_owned(),
            body_bytes: zeroize::Zeroizing::new(b"sensitive request body".to_vec()),
            idempotency_key: "sensitive-idempotency-key".to_owned(),
            started: tokio::time::Instant::now(),
            deadline: tokio::time::Instant::now() + RETRY_BUDGET,
            retries: 0,
            credentials,
            unauthorized: false,
            turn_state: None,
            turn_state_from_header: false,
            wire_bytes: 0,
            test_credentials: None,
        });
        turn.response_attempt_replacement = true;
        turn.model_event_emitted = true;
        turn.append_text_emitted = true;
        turn.pending_failure = Some(ResponseFailure {
            message: "retry me".to_owned(),
            retriable: true,
            retry_after: Some(Duration::ZERO),
        });
        let controller = CancellationController::new();
        let cancellation = controller.handle().checkpoint();
        controller.interrupt();

        let marker = turn
            .next_event(Some(cancellation.clone()))
            .await
            .unwrap()
            .expect("required marker");
        assert!(matches!(
            marker,
            ModelTurnEvent::Delta(ref delta) if crate::response_attempt::is_marker(delta)
        ));
        assert!(matches!(
            turn.next_event(Some(cancellation)).await,
            Err(LoopError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancelled_reopen_is_not_retryable_before_first_outward_event() {
        let mut turn = OpenAiSubscriptionTurn::new(stream::empty(), "gpt-5.4".into(), None);
        let credentials = super::auth::TokenRecord::for_test("sensitive-access", "test-account");
        let binding = credentials.binding().unwrap();
        turn.request_context = Some(super::OpenAiSubscriptionRequestContext {
            config: SubscriptionConfig::new("gpt-5.4".into()).unwrap(),
            client: reqwest::Client::new(),
            binding,
            session_id: "sensitive-session".to_owned(),
            body_bytes: zeroize::Zeroizing::new(b"sensitive request body".to_vec()),
            idempotency_key: "sensitive-idempotency-key".to_owned(),
            started: tokio::time::Instant::now(),
            deadline: tokio::time::Instant::now() + RETRY_BUDGET,
            retries: 0,
            credentials,
            unauthorized: false,
            turn_state: None,
            turn_state_from_header: false,
            wire_bytes: 0,
            test_credentials: None,
        });
        turn.retryable_transport_failure = true;
        let controller = CancellationController::new();
        let cancellation = controller.handle().checkpoint();
        controller.interrupt();

        assert!(
            turn.schedule_reopen(
                LoopError::Provider("retryable stream failure".to_owned()),
                None,
            )
            .unwrap()
            .is_none()
        );
        assert!(matches!(
            turn.reopen_stream(Some(cancellation)).await,
            Err(LoopError::Cancelled)
        ));
        assert!(!turn.model_event_emitted);
        assert!(!turn.retryable_transport_failure);
        assert!(turn.request_context.is_some());
        assert!(turn.pending_reopen.is_none());
    }

    #[tokio::test]
    async fn cancellation_wins_over_prefetched_output() {
        let chunk = Bytes::from_static(
            br#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_test_123"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"item-1"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":2,"item_id":"item-1","output_index":0,"content_index":0,"delta":"hello"}

"#,
        );
        let mut turn = OpenAiSubscriptionTurn::new(
            stream::iter([Ok::<_, reqwest::Error>(chunk)]),
            "gpt-5.4".into(),
            None,
        );
        let controller = CancellationController::new();
        let cancellation = controller.handle().checkpoint();

        assert!(matches!(
            turn.next_event(Some(cancellation.clone())).await.unwrap(),
            Some(ModelTurnEvent::Delta(_))
        ));
        let credentials = super::auth::TokenRecord::for_test("sensitive-access", "test-account");
        let binding = credentials.binding().unwrap();
        turn.request_context = Some(super::OpenAiSubscriptionRequestContext {
            config: SubscriptionConfig::new("gpt-5.4".into()).unwrap(),
            client: reqwest::Client::new(),
            binding,
            session_id: "sensitive-session".to_owned(),
            body_bytes: zeroize::Zeroizing::new(b"sensitive request body".to_vec()),
            idempotency_key: "sensitive-idempotency-key".to_owned(),
            started: tokio::time::Instant::now(),
            deadline: tokio::time::Instant::now() + RETRY_BUDGET,
            retries: 0,
            credentials,
            unauthorized: false,
            turn_state: None,
            turn_state_from_header: false,
            wire_bytes: turn.wire_bytes,
            test_credentials: None,
        });
        zeroizing_extend(
            &mut turn.buffer,
            b"sensitive partial parser bytes",
            MAX_STREAM_BYTES,
            "test limit",
        )
        .unwrap();
        controller.interrupt();

        assert!(matches!(
            turn.next_event(Some(cancellation)).await,
            Err(LoopError::Cancelled)
        ));
        assert!(turn.request_context.is_none());
        assert!(turn.buffer.is_empty());
    }

    #[test]
    fn parses_reported_model_context_windows() {
        let windows = parse_context_windows(&json!({
            "models": [
                {"slug": "gpt-5.4", "context_window": 272_000},
                {"slug": "gpt-no-window", "context_window": null},
                {"slug": [], "context_window": "future-format"}
            ]
        }))
        .expect("valid catalog");

        assert_eq!(windows.get("gpt-5.4"), Some(&272_000));
        assert!(!windows.contains_key("gpt-no-window"));
    }

    #[test]
    fn usage_keeps_provider_totals_for_context_occupancy() {
        let usage = parse_usage(
            &json!({
                "input_tokens": 50_000,
                "output_tokens": 3_000,
                "input_tokens_details": {"cached_tokens": 40_000},
                "output_tokens_details": {"reasoning_tokens": 2_000}
            }),
            Some(272_000),
        )
        .expect("valid usage");
        let tokens = usage.tokens.expect("token usage");

        assert_eq!(tokens.input_tokens, 50_000);
        assert_eq!(tokens.output_tokens, 3_000);
        assert_eq!(tokens.cached_input_tokens, Some(40_000));
        assert_eq!(tokens.reasoning_tokens, Some(2_000));
        assert_eq!(usage.metadata.get("context_used"), Some(&json!(53_000)));
        assert_eq!(usage.metadata.get("context_window"), Some(&json!(272_000)));
    }

    #[test]
    fn rejects_usage_details_larger_than_their_totals() {
        assert!(
            parse_usage(
                &json!({
                    "input_tokens": 10,
                    "output_tokens": 2,
                    "input_tokens_details": {"cached_tokens": 11}
                }),
                Some(272_000),
            )
            .is_err()
        );
        assert!(
            parse_usage(
                &json!({
                    "input_tokens": 10,
                    "output_tokens": 2,
                    "output_tokens_details": {"reasoning_tokens": 3}
                }),
                Some(272_000),
            )
            .is_err()
        );
    }

    #[test]
    fn usage_without_a_reported_window_keeps_token_accounting() {
        let usage = parse_usage(&json!({"input_tokens": 10, "output_tokens": 2}), None)
            .expect("valid usage");

        assert_eq!(usage.metadata.get("context_used"), Some(&json!(12)));
        assert!(!usage.metadata.contains_key("context_window"));
    }
}
