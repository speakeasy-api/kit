use std::{collections::HashMap, io::Cursor, sync::Arc, time::Duration};

use agentkit_core::{DataRef, ItemKind, MetadataMap, Modality, Part, ToolOutput};
use agentkit_http::{
    Authentication, AuthenticationAttempt, AuthenticationProvider, HeaderMap, HeaderValue,
    HttpClient, HttpError, HttpRequest, HttpResponse, ResilienceConfig,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, SessionConfig, TurnRequest,
};
use agentkit_provider_openai::{
    OpenAIResponsesAdapter, OpenAIResponsesConfig, OpenAIResponsesLimits, OpenAIResponsesProfile,
    OpenAIResponsesSession, OpenAIResponsesTurn as UpstreamOpenAIResponsesTurn,
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt as _;
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgb, RgbImage,
    codecs::jpeg::JpegEncoder, imageops::FilterType,
};
use serde_json::Value;
use zeroize::Zeroizing;

use super::{
    adapter::{stamp_context_window, valid_model_id},
    openai_auth as auth,
};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODELS_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const MODEL_CATALOG_CLIENT_VERSION: &str = "0.144.0";
const MAX_MODELS_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 1_000;
const MAX_CATALOG_MODEL_ID_BYTES: usize = 128;
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_ATTEMPT_BYTES: usize = 16 * 1024 * 1024;
const MAX_WIRE_BYTES: usize = 4 * MAX_ATTEMPT_BYTES;
const MAX_ITEMS: usize = 10_000;
const MAX_FIELD_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_DECODED_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 10_000_000;
const MAX_IMAGE_DIMENSION: u32 = 8_192;
const MAX_TOOL_RESULT_DEPTH: usize = 8;
const JPEG_DATA_URL_PREFIX: &str = "data:image/jpeg;base64,";
const MAX_NORMALIZED_IMAGE_BYTES: usize = ((MAX_FIELD_BYTES - JPEG_DATA_URL_PREFIX.len()) / 4) * 3;
const MAX_SERVER_DELAY: Duration = Duration::from_secs(10 * 60);
const MAX_SUBSCRIPTION_AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const LEGACY_CONTINUATION_METADATA: &str = "openai.subscription.v1";
const CONTINUATION_METADATA: &str = "openai.responses.continuation.v1";

fn subscription_resilience() -> ResilienceConfig {
    ResilienceConfig {
        max_retries: usize::MAX,
        retry_budget: Duration::from_secs(24 * 60 * 60),
        attempt_timeout: Some(Duration::from_secs(10 * 60)),
        stream_idle_timeout: Some(Duration::from_secs(5 * 60)),
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(60),
    }
}

#[derive(Clone, Debug)]
pub struct SubscriptionConfig {
    pub model: String,
    pub credential_storage: crate::credentials::CredentialStorage,
    #[cfg(test)]
    endpoint: Option<String>,
}

impl SubscriptionConfig {
    pub fn new(model: String) -> Result<Self, String> {
        if !valid_model_id(&model) {
            return Err("model name is outside canonical bounds".into());
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
        storage: crate::credentials::CredentialStorage,
    ) -> Self {
        self.credential_storage = storage;
        self
    }

    fn endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            return endpoint;
        }
        ENDPOINT
    }
}

#[derive(Clone)]
pub struct OpenAiSubscriptionAdapter {
    config: SubscriptionConfig,
    reasoning_effort: Option<super::adapter::ReasoningEffort>,
    catalog_client: reqwest::Client,
    responses_client: agentkit_http::Http,
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
            .map_err(|_| "could not build OpenAI subscription client".to_owned())?;
        let catalog_client = client.clone();
        let responses_client = agentkit_http::Http::new(ChatGptRetryHintsClient(client));
        Ok(Self {
            config,
            reasoning_effort,
            catalog_client,
            responses_client,
            context_windows: Arc::new(tokio::sync::OnceCell::new()),
        })
    }
}

#[async_trait]
impl ModelAdapter for OpenAiSubscriptionAdapter {
    type Session = OpenAiSubscriptionSession;

    async fn start_session(&self, session: SessionConfig) -> Result<Self::Session, LoopError> {
        let session_id = session.session_id.to_string();
        if session_id.is_empty() || session_id.len() > 256 || !session_id.is_ascii() {
            return Err(protocol("session ID is outside canonical bounds"));
        }
        let resilience = subscription_resilience();
        let credentials = load_credentials(
            self.config.credential_storage.clone(),
            auth_timeout(&resilience),
        )
        .await?;
        let binding = credentials
            .binding()
            .map_err(|error| LoopError::Provider(error.to_string()))?;
        let authentication_binding = binding_string(&binding);
        // Catalog discovery stays independent and best-effort.
        let context_windows = self
            .context_windows
            .get_or_try_init(|| async {
                fetch_context_windows(&self.catalog_client, &credentials)
                    .await
                    .map(Arc::new)
            })
            .await
            .cloned()
            .unwrap_or_default();
        let authentication = Authentication::new(OpenAiAuthenticationProvider {
            credential_storage: self.config.credential_storage.clone(),
            binding,
            timeout: auth_timeout(&resilience),
        });
        let mut config =
            OpenAIResponsesConfig::chatgpt_private(self.config.model.clone(), authentication)
                .with_endpoint(self.config.endpoint())
                .with_originator("kit")
                .with_user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
                .with_limits(OpenAIResponsesLimits {
                    max_request_bytes: MAX_REQUEST_BYTES,
                    max_attempt_bytes: MAX_ATTEMPT_BYTES,
                    max_wire_bytes: MAX_WIRE_BYTES,
                    max_items: MAX_ITEMS,
                    max_text_bytes: MAX_FIELD_BYTES,
                })
                .with_resilience(resilience);
        debug_assert_eq!(config.profile, OpenAIResponsesProfile::ChatGptPrivate);
        if let Some(effort) = self.reasoning_effort {
            config = config.with_reasoning_effort(effort.as_str());
        }
        let inner = OpenAIResponsesAdapter::with_client(config, self.responses_client.clone())
            .start_session(session)
            .await?;
        Ok(OpenAiSubscriptionSession {
            inner,
            context_window: context_windows.get(&self.config.model).copied(),
            model: self.config.model.clone(),
            authentication_binding,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("openai-subscription")
    }
}

pub struct OpenAiSubscriptionSession {
    inner: OpenAIResponsesSession,
    context_window: Option<u64>,
    model: String,
    authentication_binding: String,
}

#[async_trait]
impl ModelSession for OpenAiSubscriptionSession {
    type Turn = OpenAiSubscriptionTurn;
    async fn begin_turn(
        &mut self,
        mut request: TurnRequest,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        migrate_legacy_continuations(&mut request, &self.model, &self.authentication_binding)?;
        let normalization_cancellation = cancellation.clone();
        let request = tokio::task::spawn_blocking(move || {
            normalize_openai_images(request, normalization_cancellation.as_ref())
        })
        .await
        .map_err(|_| protocol("Responses image normalization task failed"))??;
        self.inner
            .begin_turn(request, cancellation)
            .await
            .map(|inner| OpenAiSubscriptionTurn {
                inner,
                context_window: self.context_window,
            })
    }
    fn model_name(&self) -> Option<&str> {
        self.inner.model_name()
    }
    fn provider_name(&self) -> Option<&str> {
        Some("openai-subscription")
    }
}

fn normalize_openai_images(
    mut request: TurnRequest,
    cancellation: Option<&agentkit_core::TurnCancellation>,
) -> Result<TurnRequest, LoopError> {
    for item in &mut request.transcript {
        check_image_cancellation(cancellation)?;
        if matches!(
            item.kind,
            ItemKind::User | ItemKind::Context | ItemKind::Tool
        ) {
            normalize_openai_parts(&mut item.parts, cancellation, 0)?;
        }
    }
    Ok(request)
}

fn normalize_openai_parts(
    parts: &mut [Part],
    cancellation: Option<&agentkit_core::TurnCancellation>,
    depth: usize,
) -> Result<(), LoopError> {
    if depth > MAX_TOOL_RESULT_DEPTH {
        return Err(protocol("Responses tool-result media nesting is too deep"));
    }
    for part in parts {
        check_image_cancellation(cancellation)?;
        match part {
            Part::Media(media) if media.modality == Modality::Image => {
                if serialized_image_bytes(&media.data, &media.mime_type)
                    .is_some_and(|bytes| bytes > MAX_FIELD_BYTES)
                {
                    let source = inline_image_bytes(&media.data, &media.mime_type)?
                        .ok_or_else(|| protocol("Responses oversized image is not inline data"))?;
                    let normalized = normalize_image_bytes(source, &media.mime_type, cancellation)?;
                    media.mime_type = "image/jpeg".into();
                    media.data = DataRef::InlineBytes(normalized);
                }
            }
            Part::ToolResult(result) => {
                if let ToolOutput::Parts(parts) = &mut result.output {
                    normalize_openai_parts(parts, cancellation, depth + 1)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_image_cancellation(
    cancellation: Option<&agentkit_core::TurnCancellation>,
) -> Result<(), LoopError> {
    if cancellation.is_some_and(agentkit_core::TurnCancellation::is_cancelled) {
        Err(LoopError::Cancelled)
    } else {
        Ok(())
    }
}

fn serialized_image_bytes(data: &DataRef, mime_type: &str) -> Option<usize> {
    let prefix = format!("data:{mime_type};base64,").len();
    match data {
        DataRef::InlineBytes(bytes) => Some(prefix + bytes.len().div_ceil(3) * 4),
        DataRef::InlineText(text) if text.starts_with("data:") => Some(text.len()),
        DataRef::InlineText(text) => Some(prefix + text.len()),
        DataRef::Uri(uri) if uri.starts_with("data:") => Some(uri.len()),
        DataRef::Uri(_) | DataRef::Handle(_) => None,
    }
}

fn inline_image_bytes(data: &DataRef, mime_type: &str) -> Result<Option<Vec<u8>>, LoopError> {
    if let DataRef::InlineBytes(bytes) = data {
        if bytes.len() > MAX_SOURCE_IMAGE_BYTES {
            return Err(protocol("Responses image exceeds the 10 MiB source limit"));
        }
        return Ok(Some(bytes.clone()));
    }

    let text = match data {
        DataRef::InlineText(text) | DataRef::Uri(text) if text.starts_with("data:") => text
            .strip_prefix(&format!("data:{mime_type};base64,"))
            .ok_or_else(|| protocol("Responses image data URL is not canonical base64"))?,
        DataRef::InlineText(text) => text,
        DataRef::Uri(_) | DataRef::Handle(_) => return Ok(None),
        DataRef::InlineBytes(_) => unreachable!("handled above"),
    };
    let max_base64_bytes = MAX_SOURCE_IMAGE_BYTES.div_ceil(3) * 4;
    if text.len() > max_base64_bytes {
        return Err(protocol("Responses image exceeds the 10 MiB source limit"));
    }
    let bytes = BASE64
        .decode(text)
        .map_err(|_| protocol("Responses image is not valid base64"))?;
    if bytes.len() > MAX_SOURCE_IMAGE_BYTES {
        return Err(protocol("Responses image exceeds the 10 MiB source limit"));
    }
    Ok(Some(bytes))
}

fn normalize_image_bytes(
    bytes: Vec<u8>,
    mime_type: &str,
    cancellation: Option<&agentkit_core::TurnCancellation>,
) -> Result<Vec<u8>, LoopError> {
    check_image_cancellation(cancellation)?;
    let mut reader = ImageReader::new(Cursor::new(bytes));
    if let Some(format) = ImageFormat::from_mime_type(mime_type) {
        reader.set_format(format);
    } else {
        reader = reader
            .with_guessed_format()
            .map_err(|_| protocol("Responses image format could not be detected"))?;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_IMAGE_BYTES);
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|_| protocol("Responses image could not be decoded"))?;
    let (width, height) = decoder.dimensions();
    if width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
        || decoder.total_bytes() > MAX_DECODED_IMAGE_BYTES
    {
        return Err(protocol("Responses image dimensions are too large"));
    }
    let orientation = decoder
        .orientation()
        .map_err(|_| protocol("Responses image orientation could not be read"))?;
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|_| protocol("Responses image could not be decoded"))?;
    image.apply_orientation(orientation);
    check_image_cancellation(cancellation)?;
    let rgba = image.into_rgba8();
    let rgb = RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let pixel = rgba.get_pixel(x, y).0;
        let alpha = u16::from(pixel[3]);
        let flatten =
            |channel: u8| ((u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        Rgb([flatten(pixel[0]), flatten(pixel[1]), flatten(pixel[2])])
    });
    encode_image_to_budget(DynamicImage::ImageRgb8(rgb), cancellation)
}

fn encode_image_to_budget(
    mut image: DynamicImage,
    cancellation: Option<&agentkit_core::TurnCancellation>,
) -> Result<Vec<u8>, LoopError> {
    const QUALITIES: [u8; 5] = [85, 75, 65, 55, 45];
    loop {
        let mut smallest = Vec::new();
        for quality in QUALITIES {
            check_image_cancellation(cancellation)?;
            let mut encoded = Vec::new();
            JpegEncoder::new_with_quality(&mut encoded, quality)
                .encode_image(&image)
                .map_err(|_| protocol("Responses image could not be encoded"))?;
            if encoded.len() <= MAX_NORMALIZED_IMAGE_BYTES {
                return Ok(encoded);
            }
            smallest = encoded;
        }

        let (width, height) = (image.width(), image.height());
        if width == 1 && height == 1 {
            return Err(protocol(
                "Responses image could not be reduced to the media limit",
            ));
        }
        let ratio = ((MAX_NORMALIZED_IMAGE_BYTES as f64 / smallest.len() as f64).sqrt() * 0.9)
            .clamp(0.5, 0.85);
        let next_width =
            ((width as f64 * ratio).floor() as u32).clamp(1, width.saturating_sub(1).max(1));
        let next_height =
            ((height as f64 * ratio).floor() as u32).clamp(1, height.saturating_sub(1).max(1));
        check_image_cancellation(cancellation)?;
        image = image.resize(next_width, next_height, FilterType::Lanczos3);
    }
}

pub struct OpenAiSubscriptionTurn {
    inner: UpstreamOpenAIResponsesTurn,
    context_window: Option<u64>,
}

#[async_trait]
impl ModelTurn for OpenAiSubscriptionTurn {
    async fn next_event(
        &mut self,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        let mut event = self.inner.next_event(cancellation).await?;
        if let Some(context_window) = self.context_window {
            stamp_context_window(
                &mut event,
                context_window,
                "openai.subscription.context_window",
            );
        }
        Ok(event)
    }
}

#[derive(Clone)]
struct ChatGptRetryHintsClient(reqwest::Client);

#[async_trait]
impl HttpClient for ChatGptRetryHintsClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        HttpClient::execute(&self.0, request)
            .await
            .map(normalize_server_delay)
    }
}

fn normalize_server_delay(response: HttpResponse) -> HttpResponse {
    let generic = agentkit_http::retry_hint(response.headers());
    let chatgpt = (response.status() == agentkit_http::StatusCode::TOO_MANY_REQUESTS)
        .then(|| {
            response
                .headers()
                .iter()
                .filter(|(name, _)| name.as_str().starts_with("x-ratelimit-reset"))
                .filter_map(|(_, value)| parse_chatgpt_reset(value.to_str().ok()?))
                .max()
        })
        .flatten();
    if chatgpt.is_some() || generic.is_some_and(|delay| delay > MAX_SERVER_DELAY) {
        let delay = chatgpt
            .or(generic)
            .unwrap_or_default()
            .min(MAX_SERVER_DELAY);
        let status = response.status();
        let final_url = response.url().to_owned();
        let mut headers = response.headers().clone();
        let reset_headers = headers
            .keys()
            .filter(|name| name.as_str().starts_with("x-ratelimit-reset"))
            .cloned()
            .collect::<Vec<_>>();
        for name in reset_headers {
            headers.remove(name);
        }
        for name in ["retry-after", "ratelimit-reset", "x-rate-limit-reset"] {
            headers.remove(name);
        }
        let value = HeaderValue::from_str(&delay.as_secs_f64().to_string())
            .expect("bounded retry delay is a valid header value");
        headers.insert("retry-after", value);
        return HttpResponse::new(status, headers, final_url, response.bytes_stream());
    }
    response
}

fn parse_chatgpt_reset(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(number) = value.parse::<f64>() {
        if !number.is_finite() || number < 0.0 {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        let seconds = if number >= 1e12 {
            number / 1000.0 - now
        } else if number >= 1e9 {
            number - now
        } else {
            number
        };
        return Duration::try_from_secs_f64(seconds.max(0.0)).ok();
    }
    let mut total = Duration::ZERO;
    let mut rest = value;
    while !rest.is_empty() {
        let number_len = rest
            .find(|character: char| !character.is_ascii_digit() && character != '.')
            .filter(|length| *length > 0)?;
        let number = rest[..number_len].parse::<f64>().ok()?;
        rest = &rest[number_len..];
        let unit_len = rest
            .find(|character: char| character.is_ascii_digit() || character == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit_len] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => return None,
        };
        total = total.saturating_add(Duration::try_from_secs_f64(number * seconds).ok()?);
        rest = &rest[unit_len..];
    }
    (!value.is_empty()).then_some(total)
}

#[derive(Clone)]
struct OpenAiAuthenticationProvider {
    credential_storage: crate::credentials::CredentialStorage,
    binding: auth::CredentialBinding,
    timeout: Duration,
}

#[async_trait]
impl AuthenticationProvider for OpenAiAuthenticationProvider {
    async fn authenticate(
        &self,
        previous: Option<&AuthenticationAttempt>,
    ) -> Result<AuthenticationAttempt, HttpError> {
        let rejected = previous
            .map(|attempt| {
                attempt
                    .state::<auth::TokenRecord>()
                    .cloned()
                    .ok_or_else(|| {
                        HttpError::Other("OpenAI authentication attempt state is invalid".into())
                    })
            })
            .transpose()?;
        let storage = self.credential_storage.clone();
        let timeout = self.timeout;
        let record = tokio::task::spawn_blocking(move || {
            let deadline = auth::checked_deadline(timeout)?;
            match rejected {
                Some(record) => {
                    auth::refresh_after_unauthorized(&storage, record.access_token(), deadline)
                }
                None => auth::access_token(&storage, deadline),
            }
        })
        .await
        .map_err(|_| HttpError::Other("OpenAI authentication worker failed".into()))?
        .map_err(|error| HttpError::Other(error.to_string()))?;
        ensure_credential_binding(&self.binding, &record).map_err(HttpError::Other)?;
        authentication_attempt(record)
    }
}

fn auth_timeout(config: &ResilienceConfig) -> Duration {
    config
        .attempt_timeout
        .unwrap_or(config.retry_budget)
        .min(config.retry_budget)
        .min(MAX_SUBSCRIPTION_AUTH_TIMEOUT)
}

fn legacy_continuation_matches_authentication(
    account_binding: &Value,
    authentication_binding: &str,
) -> bool {
    let Some(account_binding) = account_binding.as_object() else {
        return false;
    };
    let Some(account_digest) = account_binding
        .get("account_id_digest")
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Some(generation) = account_binding
        .get("login_generation")
        .and_then(Value::as_str)
    else {
        return false;
    };
    authentication_binding == format!("openai-chatgpt-v1:{account_digest}:{generation}")
}

fn migrate_legacy_continuations(
    request: &mut TurnRequest,
    model: &str,
    authentication_binding: &str,
) -> Result<(), LoopError> {
    let session_id = request.session_id.to_string();
    for item in &mut request.transcript {
        for part in &mut item.parts {
            let Some((metadata, expected_kind, encrypted_required)) =
                continuation_metadata_mut(part)
            else {
                continue;
            };
            migrate_legacy_continuation(
                metadata,
                model,
                &session_id,
                authentication_binding,
                expected_kind,
                encrypted_required,
            )?;
        }
    }
    Ok(())
}

fn continuation_metadata_mut(part: &mut Part) -> Option<(&mut MetadataMap, &'static str, bool)> {
    match part {
        Part::ToolCall(call) => Some((&mut call.metadata, "function_call", false)),
        Part::Reasoning(reasoning) => Some((&mut reasoning.metadata, "reasoning", true)),
        Part::Media(media) => Some((&mut media.metadata, "image_generation_call", false)),
        _ => None,
    }
}

fn migrate_legacy_continuation(
    metadata: &mut MetadataMap,
    model: &str,
    session_id: &str,
    authentication_binding: &str,
    expected_kind: &str,
    encrypted_required: bool,
) -> Result<(), LoopError> {
    if metadata.contains_key(CONTINUATION_METADATA) {
        metadata.remove(LEGACY_CONTINUATION_METADATA);
        return Ok(());
    }
    let Some(raw) = metadata.get(LEGACY_CONTINUATION_METADATA).cloned() else {
        return Ok(());
    };
    let object = raw
        .as_object()
        .ok_or_else(|| protocol("legacy Responses continuation metadata is not an object"))?;
    let expected_len = if encrypted_required { 9 } else { 8 };
    if object.len() != expected_len
        || object.get("schema_version").and_then(Value::as_u64) != Some(1)
        || object.get("kind").and_then(Value::as_str) != Some(expected_kind)
        || object.get("model").and_then(Value::as_str) != Some(model)
        || object.get("session_id").and_then(Value::as_str) != Some(session_id)
        || object.get("output_index").and_then(Value::as_u64).is_none()
    {
        return Err(protocol(
            "legacy Responses continuation metadata binding is invalid",
        ));
    }
    let account_binding = object
        .get("account_binding")
        .and_then(Value::as_object)
        .filter(|binding| binding.len() == 2)
        .ok_or_else(|| protocol("legacy Responses account binding is invalid"))?;
    let digest = bounded_legacy_string(
        account_binding.get("account_id_digest"),
        "legacy account digest",
    )?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(protocol("legacy Responses account digest is invalid"));
    }
    bounded_legacy_string(
        account_binding.get("login_generation"),
        "legacy login generation",
    )?;
    bounded_legacy_string(object.get("response_id"), "legacy response_id")?;
    let item_id = bounded_legacy_string(object.get("item_id"), "legacy item_id")?;
    let encrypted_content = object.get("encrypted_content").and_then(Value::as_str);
    if encrypted_required
        && encrypted_content.is_none_or(|value| value.is_empty() || value.len() > MAX_FIELD_BYTES)
    {
        return Err(protocol(
            "legacy Responses encrypted continuation is invalid",
        ));
    }

    metadata.remove(LEGACY_CONTINUATION_METADATA);
    let account_binding = Value::Object(account_binding.clone());
    if !legacy_continuation_matches_authentication(&account_binding, authentication_binding) {
        return Ok(());
    }
    let mut migrated = serde_json::json!({
        "schema_version": 3,
        "authentication_binding": authentication_binding,
        "model": model,
        "session_id": session_id,
        "item_id": item_id,
        "kind": expected_kind,
    });
    if let Some(encrypted_content) = encrypted_content {
        migrated["encrypted_content"] = Value::String(encrypted_content.to_owned());
    }
    metadata.insert(CONTINUATION_METADATA.into(), migrated);
    Ok(())
}

fn bounded_legacy_string<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a str, LoopError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or_else(|| protocol(&format!("Responses continuation {name} is invalid")))
}

async fn load_credentials(
    storage: crate::credentials::CredentialStorage,
    timeout: Duration,
) -> Result<auth::TokenRecord, LoopError> {
    tokio::task::spawn_blocking(move || {
        let deadline = auth::checked_deadline(timeout)?;
        auth::access_token(&storage, deadline)
    })
    .await
    .map_err(|_| LoopError::Provider("OpenAI authentication worker failed".into()))?
    .map_err(|error| LoopError::Provider(error.to_string()))
}

fn ensure_credential_binding(
    expected: &auth::CredentialBinding,
    credentials: &auth::TokenRecord,
) -> Result<(), String> {
    let actual = credentials.binding().map_err(|error| error.to_string())?;
    if &actual == expected {
        Ok(())
    } else {
        Err("OpenAI credential account changed; start a new session".into())
    }
}

fn authentication_attempt(record: auth::TokenRecord) -> Result<AuthenticationAttempt, HttpError> {
    let binding = record
        .binding()
        .map_err(|error| HttpError::Other(error.to_string()))?;
    let account_id = record
        .account_id()
        .ok_or_else(|| HttpError::Other("OpenAI credential account is missing".into()))?;
    let bearer = Zeroizing::new(format!("Bearer {}", record.access_token()));
    let authorization = HeaderValue::from_str(&bearer)
        .map_err(|_| HttpError::InvalidHeader("authorization".into()))?;
    let account = HeaderValue::from_str(account_id)
        .map_err(|_| HttpError::InvalidHeader("ChatGPT-Account-ID".into()))?;
    let mut headers = HeaderMap::new();
    headers.insert("authorization", authorization);
    headers.insert("ChatGPT-Account-ID", account);
    Ok(AuthenticationAttempt::new(headers, record).with_binding(binding_string(&binding)))
}

fn binding_string(binding: &auth::CredentialBinding) -> String {
    let account_digest = blake3::hash(binding.account_id.as_bytes());
    format!("openai-chatgpt-v1:{account_digest}:{}", binding.generation)
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
        .map_err(|_| LoopError::Provider("model catalog transport failed".into()))?;
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
        let chunk = chunk.map_err(|_| LoopError::Provider("model catalog body failed".into()))?;
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
            .filter(|slug| slug.len() <= MAX_CATALOG_MODEL_ID_BYTES && valid_model_id(slug))
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

fn protocol(message: &str) -> LoopError {
    LoopError::Provider(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subscription_config_accepts_models_without_a_client_release() {
        assert!(SubscriptionConfig::new("gpt-future".into()).is_ok());
        assert!(SubscriptionConfig::new("not a model".into()).is_err());
    }

    #[test]
    fn normalized_jpeg_budget_accounts_for_base64_expansion() {
        let at_limit = DataRef::InlineBytes(vec![0; MAX_NORMALIZED_IMAGE_BYTES]);
        let over_limit = DataRef::InlineBytes(vec![0; MAX_NORMALIZED_IMAGE_BYTES + 1]);

        assert!(serialized_image_bytes(&at_limit, "image/jpeg").unwrap() <= MAX_FIELD_BYTES);
        assert!(serialized_image_bytes(&over_limit, "image/jpeg").unwrap() > MAX_FIELD_BYTES);
    }

    #[test]
    fn oversized_acp_png_is_normalized_before_responses_encoding() {
        let png = noisy_png(600, 600);
        let data_url = format!("data:image/png;base64,{}", BASE64.encode(&png));
        assert!(data_url.len() > MAX_FIELD_BYTES);
        let metadata = MetadataMap::from([("source".into(), json!("acp"))]);
        let mut parts = vec![Part::Media(
            agentkit_core::MediaPart::new(Modality::Image, "image/png", DataRef::Uri(data_url))
                .with_metadata(metadata.clone()),
        )];

        normalize_openai_parts(&mut parts, None, 0).unwrap();

        let Part::Media(media) = &parts[0] else {
            panic!("expected normalized media");
        };
        assert_eq!(media.mime_type, "image/jpeg");
        assert_eq!(media.metadata, metadata);
        let DataRef::InlineBytes(bytes) = &media.data else {
            panic!("expected normalized inline bytes");
        };
        assert!(bytes.len() <= MAX_NORMALIZED_IMAGE_BYTES);
        assert!(serialized_image_bytes(&media.data, &media.mime_type).unwrap() <= MAX_FIELD_BYTES);
        let decoded = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!((decoded.width(), decoded.height()), (600, 600));
    }

    #[test]
    fn image_normalization_observes_turn_cancellation() {
        let controller = agentkit_core::CancellationController::new();
        let cancellation = controller.handle().checkpoint();
        controller.interrupt();
        let mut parts = vec![Part::media(
            Modality::Image,
            "image/png",
            DataRef::InlineBytes(vec![0; MAX_NORMALIZED_IMAGE_BYTES + 1]),
        )];

        assert!(matches!(
            normalize_openai_parts(&mut parts, Some(&cancellation), 0),
            Err(LoopError::Cancelled)
        ));
    }

    fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| {
            let mut value = x
                .wrapping_mul(747_796_405)
                .wrapping_add(y.wrapping_mul(2_891_336_453))
                .wrapping_add(2_891_336_453);
            value = (value ^ (value >> 16)).wrapping_mul(2_246_822_519);
            value ^= value >> 13;
            Rgb([value as u8, (value >> 8) as u8, (value >> 16) as u8])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn subscription_caps_server_retry_hints_at_ten_minutes() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("3600"));
        let response = HttpResponse::new(
            agentkit_http::StatusCode::TOO_MANY_REQUESTS,
            headers,
            "https://chatgpt.com".into(),
            Box::pin(futures_util::stream::empty()),
        );
        let response = normalize_server_delay(response);
        assert_eq!(
            agentkit_http::retry_hint(response.headers()),
            Some(Duration::from_secs(10 * 60))
        );
    }

    #[test]
    fn subscription_normalizes_chatgpt_rate_limit_resets() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-ratelimit-reset-requests",
            HeaderValue::from_static("1.5"),
        );
        headers.insert(
            "x-ratelimit-reset-tokens",
            HeaderValue::from_static("6m30s"),
        );
        let response = HttpResponse::new(
            agentkit_http::StatusCode::TOO_MANY_REQUESTS,
            headers,
            "https://chatgpt.com".into(),
            Box::pin(futures_util::stream::empty()),
        );
        let response = normalize_server_delay(response);
        assert_eq!(
            agentkit_http::retry_hint(response.headers()),
            Some(Duration::from_secs(6 * 60 + 30))
        );
    }

    #[test]
    fn subscription_defaults_match_existing_policy() {
        let config = subscription_resilience();
        assert_eq!(config.max_retries, usize::MAX);
        assert_eq!(config.retry_budget, Duration::from_secs(24 * 60 * 60));
        assert_eq!(config.attempt_timeout, Some(Duration::from_secs(10 * 60)));
        assert_eq!(
            config.stream_idle_timeout,
            Some(Duration::from_secs(5 * 60))
        );
        assert_eq!(config.initial_backoff, Duration::from_secs(1));
        assert_eq!(config.max_backoff, Duration::from_secs(60));
        assert_eq!(MAX_SERVER_DELAY, Duration::from_secs(10 * 60));
    }

    #[test]
    fn subscription_auth_timeout_uses_effective_attempt_and_caps_at_thirty_seconds() {
        let mut config = subscription_resilience();
        assert_eq!(auth_timeout(&config), Duration::from_secs(30));

        config.attempt_timeout = Some(Duration::from_secs(5));
        assert_eq!(auth_timeout(&config), Duration::from_secs(5));

        config.attempt_timeout = None;
        config.retry_budget = Duration::from_secs(10);
        assert_eq!(auth_timeout(&config), Duration::from_secs(10));
    }

    #[test]
    fn authentication_deadline_rejects_unrepresentable_duration() {
        let error = auth::checked_deadline(Duration::MAX).unwrap_err();
        assert!(error.to_string().contains("monotonic clock range"));
    }

    #[test]
    fn legacy_continuation_binding_matches_current_authentication() {
        let digest = "a".repeat(64);
        let legacy = json!({
            "account_id_digest": digest,
            "login_generation": "generation-1",
        });
        let current = format!("openai-chatgpt-v1:{digest}:generation-1");
        assert!(legacy_continuation_matches_authentication(
            &legacy, &current
        ));
        assert!(!legacy_continuation_matches_authentication(
            &legacy,
            &format!("openai-chatgpt-v1:{digest}:generation-2"),
        ));
    }

    #[test]
    fn migrates_legacy_continuation_before_agentkit_encoding() {
        let digest = "a".repeat(64);
        let binding = format!("openai-chatgpt-v1:{digest}:generation-1");
        let legacy = json!({
            "schema_version": 1,
            "account_binding": {
                "account_id_digest": digest,
                "login_generation": "generation-1",
            },
            "model": "gpt-5.4",
            "session_id": "session-1",
            "response_id": "response-1",
            "item_id": "item-1",
            "output_index": 0,
            "kind": "function_call",
        });
        let mut metadata = MetadataMap::from([(LEGACY_CONTINUATION_METADATA.into(), legacy)]);

        migrate_legacy_continuation(
            &mut metadata,
            "gpt-5.4",
            "session-1",
            &binding,
            "function_call",
            false,
        )
        .unwrap();

        assert!(!metadata.contains_key(LEGACY_CONTINUATION_METADATA));
        assert_eq!(metadata[CONTINUATION_METADATA]["schema_version"], 3);
        assert_eq!(
            metadata[CONTINUATION_METADATA]["authentication_binding"],
            binding
        );
        assert_eq!(metadata[CONTINUATION_METADATA]["item_id"], "item-1");
    }

    #[test]
    fn authentication_attempt_is_bound_and_redacted() {
        let record =
            auth::TokenRecord::for_test_generation("secret-token", "account-1", "generation-1");
        let attempt = authentication_attempt(record).unwrap();
        assert_eq!(
            attempt.headers()["ChatGPT-Account-ID"],
            HeaderValue::from_static("account-1")
        );
        assert!(attempt.headers()["authorization"].is_sensitive());
        assert!(attempt.headers()["ChatGPT-Account-ID"].is_sensitive());
        let binding = attempt.binding().unwrap();
        assert!(binding.starts_with("openai-chatgpt-v1:"));
        assert!(!binding.contains("account-1"));
        assert!(binding.ends_with(":generation-1"));
        assert!(!format!("{attempt:?}").contains("secret-token"));
    }

    #[test]
    fn session_binding_rejects_generation_change() {
        let expected = auth::TokenRecord::for_test_generation("a", "account", "one")
            .binding()
            .unwrap();
        assert!(
            ensure_credential_binding(
                &expected,
                &auth::TokenRecord::for_test_generation("b", "account", "one")
            )
            .is_ok()
        );
        assert!(
            ensure_credential_binding(
                &expected,
                &auth::TokenRecord::for_test_generation("c", "account", "two")
            )
            .is_err()
        );
    }

    #[test]
    fn catalog_parser_preserves_valid_models() {
        let too_long = format!("g{}", "x".repeat(MAX_CATALOG_MODEL_ID_BYTES));
        let windows = parse_context_windows(&json!({"models": [
            {"slug": "gpt-5.4", "context_window": 200000},
            {"slug": "bad slug", "context_window": 1},
            {"slug": too_long, "context_window": 1}
        ]}))
        .unwrap();
        assert_eq!(windows.get("gpt-5.4"), Some(&200000));
        assert_eq!(windows.len(), 1);
    }
}
