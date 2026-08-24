use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use agentkit_adapter_completions::{CompletionsAdapter, CompletionsProvider, CompletionsSession};
use agentkit_core::{
    DataRef, Delta, Modality, Part, PartId, PartKind, ToolOutput, TurnCancellation, Usage,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, SessionConfig, TurnRequest,
};
use agentkit_provider_openrouter::{
    OpenRouterAdapter, OpenRouterConfig, OpenRouterProvider, OpenRouterRequestConfig,
    OpenRouterSession, OpenRouterTurn, ReasoningEffort as OpenRouterReasoningEffort,
};
use async_trait::async_trait;
use clap::ValueEnum;
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::Value;

use super::{
    OpenAiSubscriptionAdapter, OpenAiSubscriptionSession, OpenAiSubscriptionTurn, OpenRouterApiKey,
    SubscriptionConfig, speakeasy_auth,
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
    #[serde(rename = "speakeasy")]
    #[value(name = "speakeasy")]
    Speakeasy,
}

impl std::str::FromStr for ProviderKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai-subscription" => Ok(Self::OpenAiSubscription),
            "openrouter" => Ok(Self::OpenRouter),
            "speakeasy" => Ok(Self::Speakeasy),
            _ => Err(format!("unknown model provider {value:?}")),
        }
    }
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiSubscription => "openai-subscription",
            Self::OpenRouter => "openrouter",
            Self::Speakeasy => "speakeasy",
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn from_id(value: &str) -> Result<Option<Self>, String> {
        match value {
            "default" => Ok(None),
            "low" => Ok(Some(Self::Low)),
            "medium" => Ok(Some(Self::Medium)),
            "high" => Ok(Some(Self::High)),
            _ => Err("unknown reasoning effort".into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionSelection {
    model: ModelSelection,
    reasoning_effort: Option<ReasoningEffort>,
}

/// A per-session adapter whose selection is read only when a new model turn begins.
#[derive(Clone)]
pub struct SelectableAdapter {
    selection: Arc<Mutex<SessionSelection>>,
    credential_storage: crate::credentials::CredentialStorage,
    openrouter_api_key: Option<OpenRouterApiKey>,
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
        Self::new_with_credentials_and_effort(provider, model, credential_storage, None)
    }

    pub(crate) fn new_with_credentials_and_effort(
        provider: ProviderKind,
        model: impl Into<String>,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<Self, String> {
        Self::new_with_credentials_effort_and_openrouter_key(
            provider,
            model,
            credential_storage,
            reasoning_effort,
            None,
        )
    }

    pub(crate) fn new_with_credentials_effort_and_openrouter_key(
        provider: ProviderKind,
        model: impl Into<String>,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<ReasoningEffort>,
        openrouter_api_key: Option<OpenRouterApiKey>,
    ) -> Result<Self, String> {
        let selection = ModelSelection::new(provider, model);
        if !valid_model_id(&selection.model) {
            return Err("model name is outside canonical bounds".into());
        }
        KitAdapter::new_with_credentials_and_effort(
            selection.provider,
            selection.model.clone(),
            credential_storage.clone(),
            reasoning_effort,
            openrouter_api_key.as_ref(),
        )?;
        Ok(Self {
            selection: Arc::new(Mutex::new(SessionSelection {
                model: selection,
                reasoning_effort,
            })),
            credential_storage,
            openrouter_api_key,
        })
    }

    pub fn selection(&self) -> Result<ModelSelection, String> {
        self.selection
            .lock()
            .map(|value| value.model.clone())
            .map_err(|_| "session selection lock is poisoned".into())
    }

    pub fn reasoning_effort(&self) -> Result<Option<ReasoningEffort>, String> {
        self.selection
            .lock()
            .map(|value| value.reasoning_effort)
            .map_err(|_| "session selection lock is poisoned".into())
    }

    pub fn select(&self, selection: ModelSelection) -> Result<(), String> {
        if !valid_model_id(&selection.model) {
            return Err("model name is outside canonical bounds".into());
        }
        let reasoning_effort = self.reasoning_effort()?;
        KitAdapter::new_with_credentials_and_effort(
            selection.provider,
            selection.model.clone(),
            self.credential_storage.clone(),
            reasoning_effort,
            self.openrouter_api_key.as_ref(),
        )?;
        self.selection
            .lock()
            .map_err(|_| "session selection lock is poisoned")?
            .model = selection;
        Ok(())
    }

    pub fn select_reasoning_effort(
        &self,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<(), String> {
        let model = self.selection()?;
        KitAdapter::new_with_credentials_and_effort(
            model.provider,
            model.model,
            self.credential_storage.clone(),
            reasoning_effort,
            self.openrouter_api_key.as_ref(),
        )?;
        self.selection
            .lock()
            .map_err(|_| "session selection lock is poisoned")?
            .reasoning_effort = reasoning_effort;
        Ok(())
    }
}

pub struct SelectableSession {
    selection: Arc<Mutex<SessionSelection>>,
    credential_storage: crate::credentials::CredentialStorage,
    openrouter_api_key: Option<OpenRouterApiKey>,
    config: SessionConfig,
    active: SessionSelection,
    inner: KitSession,
}

#[async_trait]
impl ModelAdapter for SelectableAdapter {
    type Session = SelectableSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        let active = self
            .selection
            .lock()
            .map(|value| value.clone())
            .map_err(|_| LoopError::InvalidState("session selection lock is poisoned".into()))?;
        let inner = KitAdapter::new_with_credentials_and_effort(
            active.model.provider,
            active.model.model.clone(),
            self.credential_storage.clone(),
            active.reasoning_effort,
            self.openrouter_api_key.as_ref(),
        )
        .map_err(LoopError::InvalidState)?
        .start_session(config.clone())
        .await?;
        Ok(SelectableSession {
            selection: Arc::clone(&self.selection),
            credential_storage: self.credential_storage.clone(),
            openrouter_api_key: self.openrouter_api_key.clone(),
            config,
            active,
            inner,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.selection
            .lock()
            .ok()
            .map(|selection| selection.model.provider.as_str())
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
            .map_err(|_| LoopError::InvalidState("session selection lock is poisoned".into()))?;
        if selected != self.active {
            let replacement = KitAdapter::new_with_credentials_and_effort(
                selected.model.provider,
                selected.model.model.clone(),
                self.credential_storage.clone(),
                selected.reasoning_effort,
                self.openrouter_api_key.as_ref(),
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
        Some(&self.active.model.model)
    }

    fn provider_name(&self) -> Option<&str> {
        Some(self.active.model.provider.as_str())
    }
}

impl SelectableSession {
    fn replace_active(
        &mut self,
        selected: SessionSelection,
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
    Speakeasy(SpeakeasyKitAdapter),
}

#[derive(Clone)]
pub struct OpenRouterKitAdapter {
    inner: OpenRouterAdapter,
    client: reqwest::Client,
    models_url: Option<String>,
    model: String,
    context_window: Arc<tokio::sync::OnceCell<u64>>,
}

const SPEAKEASY_COMPLETIONS_URL: &str = "https://app.getgram.ai/chat/completions";

#[derive(Clone)]
pub struct SpeakeasyKitAdapter {
    provider: SpeakeasyProvider,
    client: agentkit_http::Http,
}

#[derive(Clone)]
struct SpeakeasyProvider {
    openrouter: OpenRouterProvider,
    api_key: String,
    project: String,
    chat_id: Option<String>,
}

// Matches Gram's chat.SessionIDToChatID mapping for captured agent sessions.
fn gram_chat_id(session_id: &str) -> String {
    uuid::Uuid::parse_str(session_id)
        .unwrap_or_else(|_| uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, session_id.as_bytes()))
        .to_string()
}

impl CompletionsProvider for SpeakeasyProvider {
    type Config = OpenRouterRequestConfig;

    fn provider_name(&self) -> &str {
        "Speakeasy"
    }

    fn endpoint_url(&self) -> &str {
        self.openrouter.endpoint_url()
    }

    fn config(&self) -> &Self::Config {
        self.openrouter.config()
    }

    fn preprocess_request(
        &self,
        builder: agentkit_http::HttpRequestBuilder,
    ) -> agentkit_http::HttpRequestBuilder {
        let builder = builder
            .header("Gram-Key", &self.api_key)
            .header("Gram-Project", &self.project)
            .header("X-Gram-Source", "kit")
            .header("User-Agent", concat!("kit/", env!("CARGO_PKG_VERSION")));
        match &self.chat_id {
            Some(chat_id) => builder.header("Gram-Chat-ID", chat_id),
            None => builder,
        }
    }

    fn streaming(&self) -> bool {
        self.openrouter.streaming()
    }

    fn apply_stream_options(
        &self,
        body: &mut serde_json::Map<String, Value>,
    ) -> Result<(), LoopError> {
        self.openrouter.apply_stream_options(body)
    }

    fn apply_prompt_cache(
        &self,
        body: &mut serde_json::Map<String, Value>,
        request: &TurnRequest,
    ) -> Result<(), LoopError> {
        self.openrouter
            .apply_prompt_cache(body, request)
            .map_err(|error| match error {
                LoopError::Provider(message) => {
                    LoopError::Provider(message.replacen("OpenRouter", "Speakeasy", 1))
                }
                error => error,
            })
    }
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
        Self::new_with_credentials_and_effort(provider, model, credential_storage, None, None)
    }

    fn new_with_credentials_and_effort(
        provider: ProviderKind,
        model: String,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<ReasoningEffort>,
        openrouter_api_key: Option<&OpenRouterApiKey>,
    ) -> Result<Self, String> {
        match provider {
            ProviderKind::OpenAiSubscription => {
                OpenAiSubscriptionAdapter::new_with_reasoning_effort(
                    SubscriptionConfig::new(model)?.with_credential_storage(credential_storage),
                    reasoning_effort,
                )
            }
            .map(Self::OpenAiSubscription),
            ProviderKind::OpenRouter => {
                let mut config = openrouter_config_from_env(
                    model.clone(),
                    &credential_storage,
                    openrouter_api_key,
                    |name| std::env::var(name),
                )?;
                apply_openrouter_reasoning_effort(&mut config, reasoning_effort);
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
            ProviderKind::Speakeasy => {
                let credentials = speakeasy_auth::load(&credential_storage)?.ok_or_else(|| {
                    "run `kit auth login speakeasy` before using the Speakeasy provider".to_string()
                })?;
                let mut config =
                    OpenRouterConfig::new("unused", model).with_base_url(SPEAKEASY_COMPLETIONS_URL);
                apply_openrouter_reasoning_effort(&mut config, reasoning_effort);
                let provider = SpeakeasyProvider {
                    openrouter: OpenRouterProvider::from(config),
                    api_key: credentials.api_key.clone(),
                    project: credentials.project.clone(),
                    chat_id: None,
                };
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .build()
                    .map_err(|_| "could not build Speakeasy completions client".to_string())?;
                Ok(Self::Speakeasy(SpeakeasyKitAdapter {
                    provider,
                    client: agentkit_http::Http::new(client),
                }))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedOpenRouterApiKeySource {
    Explicit,
    Environment,
    Stored,
}

fn openrouter_config_from_env(
    model: String,
    credential_storage: &crate::credentials::CredentialStorage,
    explicit_api_key: Option<&OpenRouterApiKey>,
    env: impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Result<OpenRouterConfig, String> {
    let (api_key, key_source) = match explicit_api_key {
        Some(api_key) if api_key.as_str().is_empty() => {
            return Err("--openrouter-api-key cannot be empty".into());
        }
        Some(api_key) => (
            api_key.as_str().to_owned(),
            ResolvedOpenRouterApiKeySource::Explicit,
        ),
        None => match env("OPENROUTER_API_KEY") {
            Ok(api_key) if !api_key.is_empty() => {
                (api_key, ResolvedOpenRouterApiKeySource::Environment)
            }
            _ => (
                super::openrouter_auth::load(credential_storage)?
                    .map(|record| record.api_key.clone())
                    .ok_or_else(|| {
                        "set OPENROUTER_API_KEY or run `kit auth login openrouter` before using the OpenRouter provider".to_string()
                    })?,
                ResolvedOpenRouterApiKeySource::Stored,
            ),
        },
    };
    let env_model = env("OPENROUTER_MODEL").unwrap_or_else(|_| "openrouter/auto".into());
    let mut config = OpenRouterConfig::new(api_key, env_model);
    if let Ok(app_name) = env("OPENROUTER_APP_NAME") {
        config = config.with_app_name(app_name);
    }
    if let Ok(site_url) = env("OPENROUTER_SITE_URL") {
        config = config.with_site_url(site_url);
    }
    if let Ok(base_url) = env("OPENROUTER_BASE_URL") {
        if key_source == ResolvedOpenRouterApiKeySource::Stored
            && !equivalent_openrouter_base_urls(&base_url, &config.base_url)
        {
            return Err(
                "stored OpenRouter credentials cannot be used with a noncanonical OPENROUTER_BASE_URL; set OPENROUTER_API_KEY explicitly for custom endpoints"
                    .into(),
            );
        }
        config = config.with_base_url(base_url);
    }
    if let Ok(value) = env("OPENROUTER_MAX_COMPLETION_TOKENS") {
        let parsed = value
            .parse::<u32>()
            .map_err(|_| format!("invalid max tokens: {value}"))?;
        config = config.with_max_completion_tokens(parsed);
    }
    if let Ok(value) = env("OPENROUTER_TEMPERATURE") {
        let parsed = value
            .parse::<f32>()
            .map_err(|_| format!("invalid temperature: {value}"))?;
        config = config.with_temperature(parsed);
    }
    if let Ok(value) = env("OPENROUTER_REASONING_EFFORT") {
        let effort = match value.as_str() {
            "minimal" => OpenRouterReasoningEffort::Minimal,
            "low" => OpenRouterReasoningEffort::Low,
            "medium" => OpenRouterReasoningEffort::Medium,
            "high" => OpenRouterReasoningEffort::High,
            other => OpenRouterReasoningEffort::Custom(other.to_string()),
        };
        config = config.with_reasoning_effort(effort);
    }
    config.model = model;
    Ok(config)
}

fn equivalent_openrouter_base_urls(candidate: &str, canonical: &str) -> bool {
    normalized_openrouter_base_url(candidate)
        .zip(normalized_openrouter_base_url(canonical))
        .is_some_and(|(candidate, canonical)| candidate == canonical)
}

fn normalized_openrouter_base_url(value: &str) -> Option<url::Url> {
    let mut url = url::Url::parse(value).ok()?;
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Some(url)
}

fn apply_openrouter_reasoning_effort(
    config: &mut OpenRouterConfig,
    reasoning_effort: Option<ReasoningEffort>,
) {
    if let Some(reasoning_effort) = reasoning_effort {
        config.reasoning_effort = Some(match reasoning_effort {
            ReasoningEffort::Low => OpenRouterReasoningEffort::Low,
            ReasoningEffort::Medium => OpenRouterReasoningEffort::Medium,
            ReasoningEffort::High => OpenRouterReasoningEffort::High,
        });
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
            Self::Speakeasy(adapter) => {
                let mut provider = adapter.provider.clone();
                provider.chat_id = Some(gram_chat_id(&config.session_id.to_string()));
                CompletionsAdapter::with_client(provider, adapter.client.clone())
                    .start_session(config)
                    .await
                    .map(|inner| {
                        KitSession::Speakeasy(SpeakeasyKitSession {
                            inner,
                            context_window: None,
                        })
                    })
            }
        }
    }

    fn provider_name(&self) -> Option<&str> {
        Some(match self {
            Self::OpenAiSubscription(_) => "openai-subscription",
            Self::OpenRouter(_) => "openrouter",
            Self::Speakeasy(_) => "speakeasy",
        })
    }
}

pub enum KitSession {
    OpenAiSubscription(OpenAiSubscriptionSession),
    OpenRouter(OpenRouterKitSession),
    Speakeasy(SpeakeasyKitSession),
}

pub struct OpenRouterKitSession {
    inner: OpenRouterSession,
    context_window: Option<u64>,
}

pub struct SpeakeasyKitSession {
    inner: CompletionsSession<SpeakeasyProvider>,
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
            Self::Speakeasy(session) => session
                .inner
                .begin_turn(request, cancellation)
                .await
                .map(|inner| OpenRouterKitTurn {
                    inner,
                    context_window: session.context_window,
                    media_part: None,
                    next_media: 0,
                })
                .map(KitTurn::Speakeasy),
        }
    }

    fn model_name(&self) -> Option<&str> {
        match self {
            Self::OpenAiSubscription(session) => session.model_name(),
            Self::OpenRouter(session) => session.inner.model_name(),
            Self::Speakeasy(session) => session.inner.model_name(),
        }
    }

    fn provider_name(&self) -> Option<&str> {
        match self {
            Self::OpenAiSubscription(session) => session.provider_name(),
            Self::OpenRouter(session) => session.inner.provider_name(),
            Self::Speakeasy(_) => Some("speakeasy"),
        }
    }
}

pub enum KitTurn {
    OpenAiSubscription(Box<OpenAiSubscriptionTurn>),
    OpenRouter(OpenRouterKitTurn),
    Speakeasy(OpenRouterKitTurn),
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
            Self::OpenRouter(turn) | Self::Speakeasy(turn) => {
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
    let public_url = OPENROUTER_MODELS_URL.to_string();
    let same_catalog = openrouter_url.as_deref() == Some(public_url.as_str());
    let (mut openrouter, mut speakeasy) = if same_catalog {
        let models = fetch_model_ids(&public_url)
            .await
            .unwrap_or_else(|_| openrouter_fallback());
        (models.clone(), models)
    } else {
        let openrouter_catalog = async {
            match openrouter_url {
                Some(url) => fetch_model_ids(&url)
                    .await
                    .unwrap_or_else(|_| openrouter_fallback()),
                None => openrouter_fallback(),
            }
        };
        let speakeasy_catalog = async {
            fetch_model_ids(&public_url)
                .await
                .unwrap_or_else(|_| openrouter_fallback())
        };
        tokio::join!(openrouter_catalog, speakeasy_catalog)
    };
    if current.provider == ProviderKind::OpenRouter && !openrouter.contains(&current.model) {
        openrouter.push(current.model.clone());
    }
    if current.provider == ProviderKind::Speakeasy && !speakeasy.contains(&current.model) {
        speakeasy.push(current.model.clone());
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
    speakeasy.sort();
    speakeasy.dedup();
    speakeasy.truncate(MAX_SELECTOR_MODELS);
    if current.provider == ProviderKind::Speakeasy && !speakeasy.contains(&current.model) {
        if speakeasy.len() == MAX_SELECTOR_MODELS {
            speakeasy.pop();
        }
        speakeasy.push(current.model.clone());
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
        ModelGroup {
            provider: ProviderKind::Speakeasy,
            models: speakeasy,
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
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    use agentkit_core::{
        DataRef, Delta, Item, ItemKind, MediaPart, MetadataMap, Modality, Part, PartId, PartKind,
        SessionId, TokenUsage, ToolCallId, ToolOutput, ToolResultPart, TurnId, Usage,
    };
    use agentkit_loop::{LoopError, ModelAdapter, ModelSession, SessionConfig, TurnRequest};
    use agentkit_provider_openrouter::{
        OpenRouterAdapter, OpenRouterConfig, ReasoningEffort as OpenRouterReasoningEffort,
    };
    use serde_json::json;

    use crate::credentials::CredentialStorage;

    use super::{
        KitAdapter, KitSession, ModelSelection, OPENROUTER_MODELS_URL, OpenRouterApiKey,
        OpenRouterKitSession, OpenRouterProvider, ProviderKind, ReasoningEffort, SelectableAdapter,
        SelectableSession, SessionSelection, SpeakeasyKitAdapter, SpeakeasyProvider,
        add_context_window, apply_openrouter_reasoning_effort, catalog_models_url,
        expose_background_call_ids, gram_chat_id, models_url, openrouter_config_from_env,
        parse_context_window, rewrite_openrouter_media,
    };

    #[test]
    fn openrouter_reasoning_effort_preserves_default_and_maps_explicit_value() {
        let mut config = OpenRouterConfig::new("test-key", "test/model");
        config.reasoning_effort = Some(OpenRouterReasoningEffort::Custom("env-default".into()));

        apply_openrouter_reasoning_effort(&mut config, None);
        assert_eq!(
            config.reasoning_effort,
            Some(OpenRouterReasoningEffort::Custom("env-default".into()))
        );

        apply_openrouter_reasoning_effort(&mut config, Some(ReasoningEffort::High));
        assert_eq!(
            config.reasoning_effort,
            Some(OpenRouterReasoningEffort::High)
        );
    }

    #[test]
    fn openrouter_key_source_controls_custom_base_url_access() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().join("credentials"));
        crate::provider::store_openrouter_test_credentials(&storage);

        let config = openrouter_config_from_env("selected/model".into(), &storage, None, |_| {
            Err(std::env::VarError::NotPresent)
        })
        .unwrap();
        assert_eq!(config.api_key, "test-openrouter-key");
        assert_eq!(config.model, "selected/model");

        let canonical_base_url = OpenRouterConfig::new("", "").base_url;
        let optional = BTreeMap::from([
            ("OPENROUTER_API_KEY", ""),
            ("OPENROUTER_MODEL", "ignored/env-model"),
            ("OPENROUTER_APP_NAME", "env-app"),
            ("OPENROUTER_SITE_URL", "https://example.com"),
            ("OPENROUTER_MAX_COMPLETION_TOKENS", "1234"),
            ("OPENROUTER_TEMPERATURE", "0.25"),
            ("OPENROUTER_REASONING_EFFORT", "provider-tier"),
        ]);
        let config = openrouter_config_from_env("selected/model".into(), &storage, None, |name| {
            if name == "OPENROUTER_BASE_URL" {
                return Ok(format!("{canonical_base_url}/"));
            }
            optional
                .get(name)
                .map(|value| (*value).to_string())
                .ok_or(std::env::VarError::NotPresent)
        })
        .unwrap();

        assert_eq!(config.api_key, "test-openrouter-key");
        assert_eq!(config.model, "selected/model");
        assert_eq!(config.base_url, format!("{canonical_base_url}/"));
        assert_eq!(config.app_name.as_deref(), Some("env-app"));
        assert_eq!(config.site_url.as_deref(), Some("https://example.com"));
        assert_eq!(config.max_completion_tokens, Some(1234));
        assert_eq!(config.temperature, Some(0.25));
        assert_eq!(
            config.reasoning_effort,
            Some(OpenRouterReasoningEffort::Custom("provider-tier".into()))
        );

        let error =
            openrouter_config_from_env(
                "selected/model".into(),
                &storage,
                None,
                |name| match name {
                    "OPENROUTER_BASE_URL" => Ok("https://example.com/v1".into()),
                    _ => Err(std::env::VarError::NotPresent),
                },
            )
            .unwrap_err();
        assert!(error.contains("stored OpenRouter credentials"), "{error}");
        assert!(error.contains("OPENROUTER_API_KEY"), "{error}");

        let config =
            openrouter_config_from_env(
                "selected/model".into(),
                &storage,
                None,
                |name| match name {
                    "OPENROUTER_API_KEY" => Ok("environment-key".into()),
                    "OPENROUTER_BASE_URL" => Ok("https://example.com/v1".into()),
                    _ => Err(std::env::VarError::NotPresent),
                },
            )
            .unwrap();
        assert_eq!(config.api_key, "environment-key");
        assert_eq!(config.model, "selected/model");
        assert_eq!(config.base_url, "https://example.com/v1");

        let explicit = OpenRouterApiKey::new("explicit-key");
        let config = openrouter_config_from_env(
            "selected/model".into(),
            &storage,
            Some(&explicit),
            |name| match name {
                "OPENROUTER_API_KEY" => Ok("environment-key".into()),
                "OPENROUTER_BASE_URL" => Ok("https://proxy.example/v1".into()),
                _ => Err(std::env::VarError::NotPresent),
            },
        )
        .unwrap();
        assert_eq!(config.api_key, "explicit-key");
        assert_eq!(config.base_url, "https://proxy.example/v1");

        let empty = OpenRouterApiKey::new("");
        let error =
            openrouter_config_from_env("selected/model".into(), &storage, Some(&empty), |_| {
                Err(std::env::VarError::NotPresent)
            })
            .unwrap_err();
        assert!(error.contains("cannot be empty"), "{error}");
    }

    #[test]
    fn selectable_adapter_keeps_explicit_key_across_selection_rebuilds() {
        let adapter = SelectableAdapter::new_with_credentials_effort_and_openrouter_key(
            ProviderKind::OpenRouter,
            "first/model",
            CredentialStorage::Memory,
            None,
            Some(OpenRouterApiKey::new("lifecycle-secret")),
        )
        .unwrap();
        adapter
            .select(ModelSelection::new(
                ProviderKind::OpenRouter,
                "second/model",
            ))
            .unwrap();
        adapter
            .select_reasoning_effort(Some(ReasoningEffort::High))
            .unwrap();
        let debug = format!("{:?}", adapter.openrouter_api_key);
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("lifecycle-secret"));
    }

    #[test]
    fn openrouter_errors_only_when_environment_and_storage_are_missing() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().join("credentials"));
        let error = openrouter_config_from_env("selected/model".into(), &storage, None, |_| {
            Err(std::env::VarError::NotPresent)
        })
        .unwrap_err();
        assert!(error.contains("OPENROUTER_API_KEY"));
        assert!(error.contains("kit auth login openrouter"));
    }

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

    #[tokio::test]
    async fn speakeasy_composes_openrouter_wire_format_with_gram_auth() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                let Some(headers_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers_end = headers_end + 4;
                let headers = std::str::from_utf8(&request[..headers_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if request.len() >= headers_end + content_length {
                    break;
                }
            }
            let response = json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 1,
                "model": "anthropic/claude-sonnet-4",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });

        let config = OpenRouterConfig::new(
            format!("gram_live_{}", "ab".repeat(32)),
            "anthropic/claude-sonnet-4",
        )
        .with_base_url(format!("http://{address}/chat/completions"))
        .with_streaming(false);
        let provider = SpeakeasyProvider {
            openrouter: OpenRouterProvider::from(config),
            api_key: format!("gram_live_{}", "ab".repeat(32)),
            project: "kit-test".into(),
            chat_id: None,
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let adapter = KitAdapter::Speakeasy(SpeakeasyKitAdapter {
            provider,
            client: agentkit_http::Http::new(client),
        });
        let mut session = adapter
            .start_session(SessionConfig::new("speakeasy-contract"))
            .await
            .unwrap();
        session
            .begin_turn(
                TurnRequest {
                    session_id: SessionId::new("speakeasy-contract"),
                    turn_id: TurnId::new("turn"),
                    transcript: vec![Item::text(ItemKind::User, "hello")],
                    available_tools: Vec::new(),
                    cache: None,
                    metadata: MetadataMap::new(),
                },
                None,
            )
            .await
            .unwrap();

        let request = server.join().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let headers = headers.to_ascii_lowercase();
        assert!(headers.contains("gram-key: gram_live_"));
        assert!(headers.contains("gram-project: kit-test"));
        assert!(headers.contains("x-gram-source: kit"));
        assert!(headers.contains("gram-chat-id: 15f428a5-735e-5088-9e50-5210f6365e50"));
        assert!(!headers.contains("authorization:"));
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "anthropic/claude-sonnet-4");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn gram_chat_id_matches_grams_agent_session_mapping() {
        assert_eq!(
            gram_chat_id("claude-session-1"),
            "0b3a60e2-f08b-5ddd-9bcb-f3732f6a3322"
        );
        assert_eq!(
            gram_chat_id("550e8400-e29b-41d4-a716-446655440000"),
            "550e8400-e29b-41d4-a716-446655440000"
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
        let active = SessionSelection {
            model: active,
            reasoning_effort: None,
        };
        SelectableSession {
            selection: Arc::new(Mutex::new(active.clone())),
            credential_storage: Default::default(),
            openrouter_api_key: None,
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
                SessionSelection {
                    model: selected.clone(),
                    reasoning_effort: Some(ReasoningEffort::High),
                },
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
                    SessionSelection {
                        model: selected,
                        reasoning_effort: None,
                    },
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
        assert_eq!(
            ModelSelection::from_id("speakeasy:anthropic/claude-sonnet-4")
                .unwrap()
                .provider,
            ProviderKind::Speakeasy
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
