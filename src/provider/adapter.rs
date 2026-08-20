use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use agentkit_core::{
    DataRef, Delta, Modality, Part, PartId, PartKind, ToolOutput, TurnCancellation, Usage,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, SessionConfig, TurnRequest,
};
use agentkit_provider_openrouter::{
    OpenRouterAdapter, OpenRouterConfig, OpenRouterSession, OpenRouterTurn,
};
use async_trait::async_trait;
use clap::ValueEnum;
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::Value;

use super::{
    OpenAiSubscriptionAdapter, OpenAiSubscriptionSession, OpenAiSubscriptionTurn,
    SubscriptionConfig,
};

const MAX_MODELS_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 10_000;
const MAX_SELECTOR_MODELS: usize = 2_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, ValueEnum)]
pub enum ProviderKind {
    #[default]
    #[serde(rename = "openai-subscription")]
    #[value(name = "openai-subscription")]
    OpenAiSubscription,
    #[serde(rename = "openrouter")]
    #[value(name = "openrouter")]
    OpenRouter,
}

impl std::str::FromStr for ProviderKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai-subscription" => Ok(Self::OpenAiSubscription),
            "openrouter" => Ok(Self::OpenRouter),
            _ => Err(format!("unknown model provider {value:?}")),
        }
    }
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiSubscription => "openai-subscription",
            Self::OpenRouter => "openrouter",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: ProviderKind,
    pub model: String,
}

impl ModelSelection {
    pub fn new(provider: ProviderKind, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }

    pub fn id(&self) -> String {
        format!("{}:{}", self.provider.as_str(), self.model)
    }

    pub fn from_id(value: &str) -> Result<Self, String> {
        let (provider, model) = value
            .split_once(':')
            .ok_or_else(|| "model selection must include a provider".to_string())?;
        if !valid_model_id(model) {
            return Err("model name is outside canonical bounds".into());
        }
        Ok(Self::new(provider.parse()?, model))
    }
}

fn valid_model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:/".contains(&byte))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelGroup {
    pub provider: ProviderKind,
    pub models: Vec<String>,
}

/// A per-session adapter whose selection is read only when a new model turn begins.
#[derive(Clone)]
pub struct SelectableAdapter {
    selection: Arc<Mutex<ModelSelection>>,
    credential_storage: crate::credentials::CredentialStorage,
}

impl SelectableAdapter {
    pub fn new(provider: ProviderKind, model: impl Into<String>) -> Result<Self, String> {
        Self::new_with_credentials(provider, model, Default::default())
    }

    pub(crate) fn new_with_credentials(
        provider: ProviderKind,
        model: impl Into<String>,
        credential_storage: crate::credentials::CredentialStorage,
    ) -> Result<Self, String> {
        let selection = ModelSelection::new(provider, model);
        if !valid_model_id(&selection.model) {
            return Err("model name is outside canonical bounds".into());
        }
        KitAdapter::new_with_credentials(
            selection.provider,
            selection.model.clone(),
            credential_storage.clone(),
        )?;
        Ok(Self {
            selection: Arc::new(Mutex::new(selection)),
            credential_storage,
        })
    }

    pub fn selection(&self) -> Result<ModelSelection, String> {
        self.selection
            .lock()
            .map(|value| value.clone())
            .map_err(|_| "model selection lock is poisoned".into())
    }

    pub fn select(&self, selection: ModelSelection) -> Result<(), String> {
        if !valid_model_id(&selection.model) {
            return Err("model name is outside canonical bounds".into());
        }
        KitAdapter::new_with_credentials(
            selection.provider,
            selection.model.clone(),
            self.credential_storage.clone(),
        )?;
        *self
            .selection
            .lock()
            .map_err(|_| "model selection lock is poisoned")? = selection;
        Ok(())
    }
}

pub struct SelectableSession {
    selection: Arc<Mutex<ModelSelection>>,
    credential_storage: crate::credentials::CredentialStorage,
    config: SessionConfig,
    active: ModelSelection,
    inner: KitSession,
}

#[async_trait]
impl ModelAdapter for SelectableAdapter {
    type Session = SelectableSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        let active = self.selection().map_err(LoopError::InvalidState)?;
        let inner = KitAdapter::new_with_credentials(
            active.provider,
            active.model.clone(),
            self.credential_storage.clone(),
        )
        .map_err(LoopError::InvalidState)?
        .start_session(config.clone())
        .await?;
        Ok(SelectableSession {
            selection: Arc::clone(&self.selection),
            credential_storage: self.credential_storage.clone(),
            config,
            active,
            inner,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.selection
            .lock()
            .ok()
            .map(|selection| selection.provider.as_str())
    }
}

fn expose_background_call_ids(request: &mut TurnRequest) {
    const DETACHED: &str = "is now running in the background";
    for item in &mut request.transcript {
        for part in &mut item.parts {
            let Part::ToolResult(result) = part else {
                continue;
            };
            let ToolOutput::Text(text) = &mut result.output else {
                continue;
            };
            if text.contains(DETACHED) {
                *text = format!(
                    "Tool call ID: {} is running in the background.\nIt runs until result or failure is delivered.",
                    result.call_id
                );
            }
        }
    }
}

#[async_trait]
impl ModelSession for SelectableSession {
    type Turn = KitTurn;

    async fn begin_turn(
        &mut self,
        mut request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        let selected = self
            .selection
            .lock()
            .map(|value| value.clone())
            .map_err(|_| LoopError::InvalidState("model selection lock is poisoned".into()))?;
        if selected != self.active {
            let replacement = KitAdapter::new_with_credentials(
                selected.provider,
                selected.model.clone(),
                self.credential_storage.clone(),
            )
            .map_err(LoopError::InvalidState)?
            .start_session(self.config.clone())
            .await;
            self.replace_active(selected, replacement)?;
        }
        expose_background_call_ids(&mut request);
        self.inner.begin_turn(request, cancellation).await
    }

    fn model_name(&self) -> Option<&str> {
        Some(&self.active.model)
    }

    fn provider_name(&self) -> Option<&str> {
        Some(self.active.provider.as_str())
    }
}

impl SelectableSession {
    fn replace_active(
        &mut self,
        selected: ModelSelection,
        replacement: Result<KitSession, LoopError>,
    ) -> Result<(), LoopError> {
        let replacement = replacement?;
        self.inner = replacement;
        self.active = selected;
        Ok(())
    }
}

#[derive(Clone)]
pub enum KitAdapter {
    OpenAiSubscription(OpenAiSubscriptionAdapter),
    OpenRouter(OpenRouterKitAdapter),
}

#[derive(Clone)]
pub struct OpenRouterKitAdapter {
    inner: OpenRouterAdapter,
    client: reqwest::Client,
    models_url: Option<String>,
    model: String,
    context_window: Arc<tokio::sync::OnceCell<u64>>,
}

impl KitAdapter {
    pub fn new(provider: ProviderKind, model: String) -> Result<Self, String> {
        Self::new_with_credentials(provider, model, Default::default())
    }

    pub(crate) fn new_with_credentials(
        provider: ProviderKind,
        model: String,
        credential_storage: crate::credentials::CredentialStorage,
    ) -> Result<Self, String> {
        match provider {
            ProviderKind::OpenAiSubscription => OpenAiSubscriptionAdapter::new(
                SubscriptionConfig::new(model)?.with_credential_storage(credential_storage),
            )
            .map(Self::OpenAiSubscription),
            ProviderKind::OpenRouter => {
                let mut config = OpenRouterConfig::from_env().map_err(|error| error.to_string())?;
                config.model = model.clone();
                let models_url = models_url(&config.base_url);
                let inner = OpenRouterAdapter::new(config).map_err(|error| error.to_string())?;
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(15))
                    .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|_| "could not build OpenRouter model catalog client".to_owned())?;
                Ok(Self::OpenRouter(OpenRouterKitAdapter {
                    inner,
                    client,
                    models_url,
                    model,
                    context_window: Arc::new(tokio::sync::OnceCell::new()),
                }))
            }
        }
    }
}

#[async_trait]
impl ModelAdapter for KitAdapter {
    type Session = KitSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        match self {
            Self::OpenAiSubscription(adapter) => adapter
                .start_session(config)
                .await
                .map(KitSession::OpenAiSubscription),
            Self::OpenRouter(adapter) => {
                let session = adapter.inner.start_session(config).await?;
                let context_window = match &adapter.models_url {
                    Some(url) => adapter
                        .context_window
                        .get_or_try_init(|| {
                            fetch_context_window(&adapter.client, url, &adapter.model)
                        })
                        .await
                        .ok()
                        .copied(),
                    None => None,
                };
                Ok(KitSession::OpenRouter(OpenRouterKitSession {
                    inner: session,
                    context_window,
                }))
            }
        }
    }

    fn provider_name(&self) -> Option<&str> {
        Some(match self {
            Self::OpenAiSubscription(_) => "openai-subscription",
            Self::OpenRouter(_) => "openrouter",
        })
    }
}

pub enum KitSession {
    OpenAiSubscription(OpenAiSubscriptionSession),
    OpenRouter(OpenRouterKitSession),
}

pub struct OpenRouterKitSession {
    inner: OpenRouterSession,
    context_window: Option<u64>,
}

#[async_trait]
impl ModelSession for KitSession {
    type Turn = KitTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        match self {
            Self::OpenAiSubscription(session) => session
                .begin_turn(request, cancellation)
                .await
                .map(Box::new)
                .map(KitTurn::OpenAiSubscription),
            Self::OpenRouter(session) => session
                .inner
                .begin_turn(request, cancellation)
                .await
                .map(|inner| OpenRouterKitTurn {
                    inner,
                    context_window: session.context_window,
                    media_part: None,
                    next_media: 0,
                })
                .map(KitTurn::OpenRouter),
        }
    }

    fn model_name(&self) -> Option<&str> {
        match self {
            Self::OpenAiSubscription(session) => session.model_name(),
            Self::OpenRouter(session) => session.inner.model_name(),
        }
    }

    fn provider_name(&self) -> Option<&str> {
        match self {
            Self::OpenAiSubscription(session) => session.provider_name(),
            Self::OpenRouter(session) => session.inner.provider_name(),
        }
    }
}

pub enum KitTurn {
    OpenAiSubscription(Box<OpenAiSubscriptionTurn>),
    OpenRouter(OpenRouterKitTurn),
}

pub struct OpenRouterKitTurn {
    inner: OpenRouterTurn,
    context_window: Option<u64>,
    media_part: Option<PartId>,
    next_media: usize,
}

#[async_trait]
impl ModelTurn for KitTurn {
    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        match self {
            Self::OpenAiSubscription(turn) => turn.next_event(cancellation).await,
            Self::OpenRouter(turn) => {
                let mut event = turn.inner.next_event(cancellation).await?;
                if let Some(ModelTurnEvent::Delta(delta)) = &mut event {
                    rewrite_openrouter_media(delta, &mut turn.media_part, &mut turn.next_media);
                }
                if let Some(context_window) = turn.context_window {
                    match &mut event {
                        Some(ModelTurnEvent::Usage(usage)) => {
                            add_context_window(usage, context_window);
                        }
                        Some(ModelTurnEvent::Finished(result)) => {
                            if let Some(usage) = &mut result.usage {
                                add_context_window(usage, context_window);
                            }
                            for item in &mut result.output_items {
                                if let Some(usage) = &mut item.usage {
                                    add_context_window(usage, context_window);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(event)
            }
        }
    }
}

fn rewrite_openrouter_media(
    delta: &mut Delta,
    media_part: &mut Option<PartId>,
    next_media: &mut usize,
) {
    match delta {
        Delta::BeginPart { part_id, kind } if *kind == PartKind::Media => {
            *media_part = Some(part_id.clone());
            *kind = PartKind::Text;
        }
        Delta::CommitPart {
            part: Part::Media(media),
        } => {
            *next_media += 1;
            let label = media_label(media, *next_media);
            let part_id = media_part
                .take()
                .unwrap_or_else(|| PartId::new(format!("media-{next_media}")));
            *delta = Delta::AppendText {
                part_id,
                chunk: label,
            };
        }
        _ => {}
    }
}

fn media_label(media: &agentkit_core::MediaPart, index: usize) -> String {
    let kind = match media.modality {
        Modality::Image => "Image",
        Modality::Audio => "Audio",
        Modality::Video => "Video",
        Modality::Binary => "Media",
    };
    match &media.data {
        DataRef::Uri(uri) if safe_media_uri(uri) => format!("[{kind} #{index}]({uri})"),
        _ => format!("[{kind} #{index}]"),
    }
}

fn safe_media_uri(uri: &str) -> bool {
    uri.len() <= 2_048
        && url::Url::parse(uri).is_ok_and(|uri| matches!(uri.scheme(), "file" | "http" | "https"))
}

fn add_context_window(usage: &mut Usage, context_window: u64) {
    usage
        .metadata
        .insert("context_window".into(), context_window.into());
    usage
        .metadata
        .insert("openrouter.context_length".into(), context_window.into());
}

fn models_url(completions_url: &str) -> Option<String> {
    completions_url
        .trim_end_matches('/')
        .strip_suffix("/chat/completions")
        .map(|prefix| format!("{prefix}/models"))
}

async fn fetch_context_window(
    client: &reqwest::Client,
    url: &str,
    model: &str,
) -> Result<u64, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "OpenRouter model catalog transport failed".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "OpenRouter model catalog returned {}",
            response.status()
        ));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "OpenRouter model catalog body failed".to_owned())?;
        if body.len().saturating_add(chunk.len()) > MAX_MODELS_BYTES {
            return Err("OpenRouter model catalog exceeds 2 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| "OpenRouter model catalog is not valid JSON".to_owned())?;
    parse_context_window(&value, model)
        .ok_or_else(|| format!("OpenRouter model catalog omitted context length for {model:?}"))
}

fn parse_context_window(value: &Value, model: &str) -> Option<u64> {
    let models = value.get("data")?.as_array()?;
    if models.len() > MAX_MODELS {
        return None;
    }
    models.iter().find_map(|entry| {
        (entry.get("id")?.as_str()? == model)
            .then(|| entry.get("context_length")?.as_u64())
            .flatten()
            .filter(|window| *window > 0)
    })
}

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
fn catalog_models_url(configured_base: Option<&str>) -> Option<String> {
    match configured_base {
        Some(url) => models_url(url),
        None => Some(OPENROUTER_MODELS_URL.to_string()),
    }
}

const OPENROUTER_FALLBACK: &[&str] = &[
    "anthropic/claude-sonnet-4",
    "openai/gpt-5.4",
    "google/gemini-2.5-pro",
];

/// Returns a bounded, provider-grouped catalog. Remote discovery is best effort.
pub async fn model_catalog(current: &ModelSelection) -> Vec<ModelGroup> {
    let openai = [
        "gpt-5.6-sol",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex-spark",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let openrouter_url = match std::env::var_os("OPENROUTER_BASE_URL") {
        None => catalog_models_url(None),
        Some(url) => url.to_str().and_then(|url| catalog_models_url(Some(url))),
    };
    let mut openrouter = match openrouter_url {
        Some(url) => fetch_model_ids(&url)
            .await
            .unwrap_or_else(|_| openrouter_fallback()),
        None => openrouter_fallback(),
    };
    if current.provider == ProviderKind::OpenRouter && !openrouter.contains(&current.model) {
        openrouter.push(current.model.clone());
    }
    openrouter.sort();
    openrouter.dedup();
    openrouter.truncate(MAX_SELECTOR_MODELS);
    if current.provider == ProviderKind::OpenRouter && !openrouter.contains(&current.model) {
        if openrouter.len() == MAX_SELECTOR_MODELS {
            openrouter.pop();
        }
        openrouter.push(current.model.clone());
    }
    vec![
        ModelGroup {
            provider: ProviderKind::OpenAiSubscription,
            models: openai,
        },
        ModelGroup {
            provider: ProviderKind::OpenRouter,
            models: openrouter,
        },
    ]
}

fn openrouter_fallback() -> Vec<String> {
    OPENROUTER_FALLBACK
        .iter()
        .map(|model| (*model).to_string())
        .collect()
}

async fn fetch_model_ids(url: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "could not build model catalog client".to_string())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "model catalog transport failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!("model catalog returned {}", response.status()));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "model catalog body failed".to_string())?;
        if body.len().saturating_add(chunk.len()) > MAX_MODELS_BYTES {
            return Err("model catalog exceeds 2 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| "model catalog is not valid JSON".to_string())?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "model catalog omitted data".to_string())?;
    if entries.len() > MAX_MODELS {
        return Err("model catalog has too many entries".into());
    }
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .filter(|id| valid_model_id(id))
        .take(MAX_SELECTOR_MODELS)
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agentkit_core::{
        DataRef, Delta, Item, ItemKind, MediaPart, MetadataMap, Modality, Part, PartId, PartKind,
        SessionId, TokenUsage, ToolCallId, ToolOutput, ToolResultPart, TurnId, Usage,
    };
    use agentkit_loop::{LoopError, ModelAdapter, ModelSession, SessionConfig, TurnRequest};
    use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
    use serde_json::json;

    use super::{
        KitSession, ModelSelection, OPENROUTER_MODELS_URL, OpenRouterKitSession, ProviderKind,
        SelectableAdapter, SelectableSession, add_context_window, catalog_models_url,
        expose_background_call_ids, models_url, parse_context_window, rewrite_openrouter_media,
    };

    #[test]
    fn openrouter_media_delta_becomes_a_portable_placeholder() {
        let part_id = PartId::new("generated-image");
        let mut active = None;
        let mut next = 0;
        let mut begin = Delta::BeginPart {
            part_id: part_id.clone(),
            kind: PartKind::Media,
        };
        rewrite_openrouter_media(&mut begin, &mut active, &mut next);
        assert!(matches!(
            begin,
            Delta::BeginPart {
                kind: PartKind::Text,
                ..
            }
        ));

        let mut commit = Delta::CommitPart {
            part: Part::Media(MediaPart::new(
                Modality::Image,
                "image/png",
                DataRef::Uri("https://example.com/image.png".into()),
            )),
        };
        rewrite_openrouter_media(&mut commit, &mut active, &mut next);

        assert!(matches!(
            commit,
            Delta::AppendText { part_id: id, chunk }
                if id == part_id && chunk == "[Image #1](https://example.com/image.png)"
        ));
    }

    #[test]
    fn openrouter_media_placeholder_does_not_expose_data_urls() {
        let mut active = None;
        let mut next = 0;
        let mut commit = Delta::CommitPart {
            part: Part::Media(MediaPart::new(
                Modality::Image,
                "image/png",
                DataRef::Uri("data:image/png;base64,c2VjcmV0".into()),
            )),
        };

        rewrite_openrouter_media(&mut commit, &mut active, &mut next);

        assert!(matches!(
            commit,
            Delta::AppendText { chunk, .. } if chunk == "[Image #1]"
        ));
    }

    #[test]
    fn detached_results_tell_the_model_their_call_id() {
        let mut request = TurnRequest {
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn"),
            transcript: vec![Item {
                id: None,
                kind: ItemKind::Tool,
                parts: vec![Part::ToolResult(ToolResultPart::success(
                    ToolCallId::new("call_background"),
                    ToolOutput::Text("Tool compose is now running in the background. The result will be delivered when it completes.".into()),
                ))],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }],
            available_tools: Vec::new(),
            cache: None,
            metadata: MetadataMap::new(),
        };

        expose_background_call_ids(&mut request);

        let Part::ToolResult(result) = &request.transcript[0].parts[0] else {
            panic!("expected tool result");
        };
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert_eq!(
            text,
            "Tool call ID: call_background is running in the background.\nIt runs until result or failure is delivered."
        );
    }

    #[test]
    fn selectable_adapter_reports_its_concrete_initial_provider() {
        let adapter = SelectableAdapter::new(ProviderKind::OpenAiSubscription, "gpt-5.4").unwrap();

        assert_eq!(adapter.provider_name(), Some("openai-subscription"));
    }

    async fn openrouter_session(model: &str) -> KitSession {
        let adapter = OpenRouterAdapter::new(OpenRouterConfig::new("test-key", model)).unwrap();
        let inner = adapter
            .start_session(SessionConfig::new("provider-identity-test"))
            .await
            .unwrap();
        KitSession::OpenRouter(OpenRouterKitSession {
            inner,
            context_window: None,
        })
    }

    fn selectable_session(active: ModelSelection, inner: KitSession) -> SelectableSession {
        SelectableSession {
            selection: Arc::new(Mutex::new(active.clone())),
            credential_storage: Default::default(),
            config: SessionConfig::new("provider-identity-test"),
            active,
            inner,
        }
    }

    #[tokio::test]
    async fn kit_session_delegates_initial_provider_identity() {
        let session = openrouter_session("test/initial").await;

        assert_eq!(session.provider_name(), Some("openrouter"));
    }

    #[tokio::test]
    async fn successful_session_replacement_updates_canonical_provider_identity() {
        let initial = ModelSelection::new(ProviderKind::OpenAiSubscription, "gpt-5.4");
        let mut session = selectable_session(initial, openrouter_session("test/initial").await);
        assert_eq!(session.provider_name(), Some("openai-subscription"));

        let selected = ModelSelection::new(ProviderKind::OpenRouter, "test/replacement");
        session
            .replace_active(
                selected.clone(),
                Ok(openrouter_session(&selected.model).await),
            )
            .unwrap();

        assert_eq!(session.provider_name(), Some("openrouter"));
        assert_eq!(session.model_name(), Some("test/replacement"));
    }

    #[tokio::test]
    async fn failed_session_replacement_preserves_canonical_provider_identity() {
        let initial = ModelSelection::new(ProviderKind::OpenRouter, "test/initial");
        let mut session =
            selectable_session(initial.clone(), openrouter_session(&initial.model).await);
        let selected = ModelSelection::new(ProviderKind::OpenAiSubscription, "gpt-5.4");

        assert!(
            session
                .replace_active(
                    selected,
                    Err(LoopError::Provider("replacement failed".into())),
                )
                .is_err()
        );

        assert_eq!(session.provider_name(), Some("openrouter"));
        assert_eq!(session.model_name(), Some("test/initial"));
    }

    #[test]
    fn model_selection_ids_round_trip_and_switch_atomically() {
        let adapter = SelectableAdapter::new(ProviderKind::OpenAiSubscription, "gpt-5.4").unwrap();
        let selected = ModelSelection::from_id("openai-subscription:gpt-5.4-mini").unwrap();

        adapter.select(selected.clone()).unwrap();

        assert_eq!(adapter.selection().unwrap(), selected);
        assert_eq!(
            adapter.selection().unwrap().id(),
            "openai-subscription:gpt-5.4-mini"
        );
        assert!(ModelSelection::from_id("unknown:model").is_err());
        assert!(ModelSelection::from_id("openrouter:").is_err());
    }

    #[test]
    fn invalid_switch_keeps_the_previous_selection() {
        let adapter = SelectableAdapter::new(ProviderKind::OpenAiSubscription, "gpt-5.4").unwrap();

        assert!(
            adapter
                .select(ModelSelection::new(
                    ProviderKind::OpenAiSubscription,
                    "not-supported",
                ))
                .is_err()
        );
        assert_eq!(adapter.selection().unwrap().model, "gpt-5.4");
    }

    #[test]
    fn explicit_unusable_openrouter_base_does_not_select_the_public_catalog() {
        assert_eq!(
            catalog_models_url(None).as_deref(),
            Some(OPENROUTER_MODELS_URL)
        );
        assert_eq!(catalog_models_url(Some("https://example.com/custom")), None);
    }

    #[test]
    fn derives_and_parses_openrouter_model_catalog_context() {
        assert_eq!(
            models_url("https://openrouter.ai/api/v1/chat/completions").as_deref(),
            Some("https://openrouter.ai/api/v1/models")
        );
        let catalog = json!({
            "data": [
                {"id": "other/model", "context_length": 1},
                {"id": "anthropic/claude-sonnet-4", "context_length": 200_000}
            ]
        });
        assert_eq!(
            parse_context_window(&catalog, "anthropic/claude-sonnet-4"),
            Some(200_000)
        );
        assert_eq!(parse_context_window(&catalog, "missing/model"), None);
    }

    #[test]
    fn openrouter_context_window_is_stamped_on_usage() {
        let mut usage = Usage::new(TokenUsage::default());

        add_context_window(&mut usage, 200_000);

        assert_eq!(usage.metadata["context_window"], json!(200_000));
        assert_eq!(usage.metadata["openrouter.context_length"], json!(200_000));
    }
}
