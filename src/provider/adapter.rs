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
    SubscriptionConfig, chatgpt::SubscriptionModelCatalogCache, speakeasy_auth,
};

const MAX_MODELS_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 10_000;
const MAX_SELECTOR_MODELS: usize = 2_000;
const OPENROUTER_AUTH_REQUIRED: &str = "openrouter_auth_required: set OPENROUTER_API_KEY or run `kit auth login openrouter` before using the OpenRouter provider";
const SPEAKEASY_AUTH_REQUIRED: &str =
    "speakeasy_auth_required: run `kit auth login speakeasy` before using the Speakeasy provider";

pub(crate) fn authentication_method_id(detail: &str) -> Option<&'static str> {
    [
        ("openai_auth_required:", "openai"),
        ("openrouter_auth_required:", "openrouter"),
        ("speakeasy_auth_required:", "speakeasy"),
    ]
    .into_iter()
    .find_map(|(code, method_id)| detail.contains(code).then_some(method_id))
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum ProviderKind {
    #[default]
    #[serde(rename = "openai-subscription")]
    OpenAiSubscription,
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "speakeasy")]
    Speakeasy,
}

impl ValueEnum for ProviderKind {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::OpenAiSubscription, Self::OpenRouter, Self::Speakeasy]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(match self {
            Self::OpenAiSubscription => "openai-subscription",
            Self::OpenRouter => "openrouter",
            Self::Speakeasy => "speakeasy",
        }))
    }
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

pub(super) fn valid_model_id(value: &str) -> bool {
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
    /// Provider-reported windows only; missing entries are unknown.
    pub context_windows: std::collections::HashMap<String, u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ValueEnum for ReasoningEffort {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Low, Self::Medium, Self::High]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }))
    }
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
    revision: u64,
}

/// A per-session adapter whose selection is read only when a new model turn begins.
#[derive(Clone)]
pub struct SelectableAdapter {
    selection: Arc<Mutex<SessionSelection>>,
    credential_storage: crate::credentials::CredentialStorage,
    openrouter_api_key: Option<OpenRouterApiKey>,
    openai_model_catalog: SubscriptionModelCatalogCache,
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
        Ok(Self {
            selection: Arc::new(Mutex::new(SessionSelection {
                model: selection,
                reasoning_effort,
                revision: 0,
            })),
            credential_storage,
            openrouter_api_key,
            openai_model_catalog: SubscriptionModelCatalogCache::default(),
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

    async fn discovered_openai_models(&self) -> DiscoveredModels {
        let config = match SubscriptionConfig::new(OPENAI_FALLBACK[0].to_string()) {
            Ok(config) => config.with_credential_storage(self.credential_storage.clone()),
            Err(_) => return DiscoveredModels::default(),
        };
        let adapter = match OpenAiSubscriptionAdapter::new_with_reasoning_effort_and_catalog(
            config,
            None,
            self.openai_model_catalog.clone(),
        ) {
            Ok(adapter) => adapter,
            Err(_) => return DiscoveredModels::default(),
        };
        adapter
            .model_catalog()
            .await
            .map(|catalog| DiscoveredModels {
                models: catalog.visible_models().to_vec(),
                context_windows: catalog.context_windows().clone(),
            })
            .unwrap_or_default()
    }

    pub async fn model_catalog(&self, current: &ModelSelection) -> Vec<ModelGroup> {
        model_catalog_with_openai(current, self.discovered_openai_models()).await
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
        let mut current = self
            .selection
            .lock()
            .map_err(|_| "session selection lock is poisoned")?;
        current.model = selection;
        current.revision = current.revision.wrapping_add(1);
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
        let mut current = self
            .selection
            .lock()
            .map_err(|_| "session selection lock is poisoned")?;
        current.reasoning_effort = reasoning_effort;
        current.revision = current.revision.wrapping_add(1);
        Ok(())
    }
}

pub struct SelectableSession {
    selection: Arc<Mutex<SessionSelection>>,
    credential_storage: crate::credentials::CredentialStorage,
    openrouter_api_key: Option<OpenRouterApiKey>,
    openai_model_catalog: SubscriptionModelCatalogCache,
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
        let inner = KitAdapter::new_with_credentials_effort_and_catalog(
            active.model.provider,
            active.model.model.clone(),
            self.credential_storage.clone(),
            active.reasoning_effort,
            self.openrouter_api_key.as_ref(),
            self.openai_model_catalog.clone(),
        )
        .map_err(LoopError::InvalidState)?
        .start_session(config.clone())
        .await?;
        Ok(SelectableSession {
            selection: Arc::clone(&self.selection),
            credential_storage: self.credential_storage.clone(),
            openrouter_api_key: self.openrouter_api_key.clone(),
            openai_model_catalog: self.openai_model_catalog.clone(),
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
                    "Tool call ID: {} is running in the background.\nNo independent work left? STOP.",
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
            let replacement = KitAdapter::new_with_credentials_effort_and_catalog(
                selected.model.provider,
                selected.model.model.clone(),
                self.credential_storage.clone(),
                selected.reasoning_effort,
                self.openrouter_api_key.as_ref(),
                self.openai_model_catalog.clone(),
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
        self.inner.model_name()
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
    Speakeasy(Box<SpeakeasyKitAdapter>),
}

#[derive(Clone)]
pub struct OpenRouterKitAdapter {
    inner: OpenRouterAdapter,
    config: Box<OpenRouterConfig>,
    client: reqwest::Client,
    models_url: Option<String>,
    model: String,
    context_window: Arc<tokio::sync::OnceCell<OpenRouterModelInfo>>,
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
        Self::new_with_credentials_effort_and_catalog(
            provider,
            model,
            credential_storage,
            reasoning_effort,
            openrouter_api_key,
            SubscriptionModelCatalogCache::default(),
        )
    }

    fn new_with_credentials_effort_and_catalog(
        provider: ProviderKind,
        model: String,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<ReasoningEffort>,
        openrouter_api_key: Option<&OpenRouterApiKey>,
        openai_model_catalog: SubscriptionModelCatalogCache,
    ) -> Result<Self, String> {
        match provider {
            ProviderKind::OpenAiSubscription => {
                let config =
                    SubscriptionConfig::new(model)?.with_credential_storage(credential_storage);
                OpenAiSubscriptionAdapter::new_with_reasoning_effort_and_catalog(
                    config,
                    reasoning_effort,
                    openai_model_catalog,
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
                let inner = OpenRouterAdapter::new(config.clone())
                    .map_err(|error| error.to_string())?
                    .with_resilience(agentkit_http::ResilienceConfig::default());
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(15))
                    .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|_| "could not build OpenRouter model catalog client".to_owned())?;
                Ok(Self::OpenRouter(OpenRouterKitAdapter {
                    inner,
                    config: Box::new(config),
                    client,
                    models_url,
                    model,
                    context_window: Arc::new(tokio::sync::OnceCell::new()),
                }))
            }
            ProviderKind::Speakeasy => {
                let credentials = speakeasy_auth::load(&credential_storage)?
                    .ok_or_else(|| SPEAKEASY_AUTH_REQUIRED.to_string())?;
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
                Ok(Self::Speakeasy(Box::new(SpeakeasyKitAdapter {
                    provider,
                    client: agentkit_http::Http::new(client),
                })))
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
                    .ok_or_else(|| OPENROUTER_AUTH_REQUIRED.to_string())?,
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
                // The OnceCell publishes one complete immutable discovery result. Failed or
                // cancelled discovery leaves it empty; no credentials or session state change.
                let discovered = match &adapter.models_url {
                    Some(url) => adapter
                        .context_window
                        .get_or_try_init(|| {
                            fetch_openrouter_model(&adapter.client, url, &adapter.model)
                        })
                        .await
                        .inspect_err(|_| tracing::warn!("OpenRouter model discovery unavailable; retaining legacy routing without native capability assertions"))
                        .ok(),
                    None => None,
                };
                let context_window = discovered.and_then(|info| info.context_window);
                let native = discover_native_image(
                    &agentkit_http::Http::new(adapter.client.clone()),
                    &adapter.config,
                    discovered,
                )
                .await?;
                let session = if let Some(capability) = &native {
                    let native_config =
                        native_generation_config((*adapter.config).clone(), capability)?;
                    let client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(Duration::from_secs(10))
                        .timeout(NATIVE_GENERATION_TIMEOUT)
                        .build()
                        .map_err(|_| {
                            LoopError::Provider("could not build native image client".into())
                        })?;
                    CompletionsAdapter::with_client(
                        OpenRouterProvider::from(native_config),
                        agentkit_http::Http::new(BoundedImageClient {
                            inner: agentkit_http::Http::new(client),
                        }),
                    )
                    .with_resilience(native_generation_resilience())
                    .start_session(config)
                    .await?
                } else {
                    adapter.inner.start_session(config).await?
                };
                Ok(KitSession::OpenRouter(OpenRouterKitSession {
                    inner: session,
                    context_window,
                    native,
                }))
            }
            Self::Speakeasy(adapter) => {
                let mut provider = adapter.provider.clone();
                provider.chat_id = Some(gram_chat_id(&config.session_id.to_string()));
                let inner = CompletionsAdapter::with_client(provider, adapter.client.clone())
                    .with_resilience(agentkit_http::ResilienceConfig::default());
                inner.start_session(config).await.map(|inner| {
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
    native: Option<NativeImageCapability>,
}

pub struct SpeakeasyKitSession {
    inner: CompletionsSession<SpeakeasyProvider>,
    context_window: Option<u64>,
}

// Bound retained assistant-image payloads before the Completions encoder expands
// them to base64. This is not a limit on the whole request, user attachments,
// or model context; per-delivery validation remains separate.
const MAX_OUTGOING_ASSISTANT_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Project only the outbound request; the caller's canonical transcript stays typed.
/// Completions (including OpenRouter) stringify tool Parts and reject assistant
/// Media, but encode ordinary user Media as image_url content. Native transports retain typed tool images,
/// but detached notification images always need ordinary user attachments.
pub(super) fn project_tool_output_images(
    mut request: TurnRequest,
    native: bool,
) -> Result<TurnRequest, LoopError> {
    // Validate before moving or recursively visiting parts. The iterator stack
    // bounds both depth and work without allocating a sibling-sized frontier.
    const MAX_NODES: usize = 100_000;
    const MAX_DEPTH: usize = 64;
    let mut visited = 0;
    let mut retained_assistant_image_bytes = 0_usize;
    let mut pending = Vec::new();
    for item in &request.transcript {
        visited += 1;
        if visited > MAX_NODES {
            return Err(tool_image_traversal_error());
        }
        // Store only one iterator per nesting level, never a transcript-wide
        // frontier or one entry per sibling. Bound both traversal and stack size.
        pending.push(item.parts.iter());
        while let Some(parts) = pending.last_mut() {
            let Some(part) = parts.next() else {
                pending.pop();
                continue;
            };
            visited += 1;
            if visited > MAX_NODES {
                return Err(tool_image_traversal_error());
            }
            if !native
                && item.kind == agentkit_core::ItemKind::Assistant
                && let Part::Media(media) = part
                && media.modality == Modality::Image
                && let DataRef::InlineBytes(bytes) = &media.data
            {
                retained_assistant_image_bytes =
                    retained_assistant_image_bytes.saturating_add(bytes.len());
                if retained_assistant_image_bytes > MAX_OUTGOING_ASSISTANT_IMAGE_BYTES {
                    return Err(LoopError::InvalidState(
                        "selected-images-not-delivered: outgoing request/history budget of 64 MiB for retained assistant-image payloads exceeded; compact history or start a fresh session with selected attachments".into(),
                    ));
                }
            }
            if let Part::ToolResult(result) = part
                && let ToolOutput::Parts(parts) = &result.output
            {
                if pending.len() >= MAX_DEPTH {
                    return Err(tool_image_traversal_error());
                }
                pending.push(parts.iter());
            }
        }
    }

    let mut transcript = Vec::with_capacity(request.transcript.len());
    let mut outstanding = std::collections::HashSet::new();
    let mut images = Vec::new();
    for mut item in request.transcript {
        // The loop has already answered detached calls with placeholders. Its
        // completion notification contains serialized ToolResultPart values,
        // not another tool answer. Even native transports must lift these images.
        if item.kind == agentkit_core::ItemKind::Notification
            && matches!(item.parts.first(), Some(Part::Text(text)) if text.text.starts_with("Background tool results: "))
        {
            for part in &mut item.parts {
                if let Part::Structured(value) = part
                    && is_detached_result(&value.value)
                {
                    project_detached_images(&mut value.value, &mut images, &mut visited, 1)?;
                }
            }
        }
        // Register the whole item before processing any answers. Calls may also
        // span multiple assistant items, and results multiple tool items.
        for part in &item.parts {
            if let Part::ToolCall(call) = part {
                outstanding.insert(call.id.clone());
            }
        }
        // The canonical completed assistant item remains typed. Lift only the
        // outbound copy's images, without synthetic text. Register all calls
        // first so images wait for the complete parallel tool-result batch.
        let mut lifted_assistant_images = false;
        if !native && item.kind == agentkit_core::ItemKind::Assistant {
            // These are delivery quotas, not cumulative transcript quotas.
            let mut assistant_image_bytes = 0;
            let mut assistant_image_count = 0;
            let mut assistant_image_pixels = 0;
            let mut supported = Vec::new();
            for part in std::mem::take(&mut item.parts) {
                if let Part::Media(media) = &part
                    && media.modality == Modality::Image
                {
                    let DataRef::InlineBytes(bytes) = &media.data else {
                        return Err(LoopError::InvalidState("selected-images-not-delivered: historical assistant images require inline PNG/JPEG bytes".into()));
                    };
                    if bytes.len() > MAX_NATIVE_IMAGE_BYTES {
                        return Err(LoopError::InvalidState("selected-images-not-delivered: historical assistant image exceeds 8 MiB".into()));
                    }
                    let (mime, pixels) = crate::managed_files::validate_provider_image(bytes)
                        .map_err(|error| LoopError::InvalidState(format!("selected-images-not-delivered: invalid historical assistant image: {error}")))?;
                    if mime != media.mime_type {
                        return Err(LoopError::InvalidState("selected-images-not-delivered: historical image MIME disagrees with bytes".into()));
                    }
                    assistant_image_count += 1;
                    assistant_image_bytes += bytes.len();
                    assistant_image_pixels += pixels;
                    if assistant_image_count > 8
                        || assistant_image_bytes > MAX_NATIVE_DELIVERY_BYTES
                        || assistant_image_pixels > 32 * 1024 * 1024
                    {
                        return Err(LoopError::InvalidState("selected-images-not-delivered: historical assistant images exceed 8 images, 16 MiB or 32 megapixels".into()));
                    }
                    images.push(part);
                    lifted_assistant_images = true;
                } else {
                    supported.push(part);
                }
            }
            item.parts = supported;
        }
        for part in &mut item.parts {
            if let Part::ToolResult(result) = part {
                project_result_images(result, &mut images, native)?;
                outstanding.remove(&result.call_id);
            }
        }
        // An image-only assistant item has no supported content left. Do not
        // emit an invalid empty assistant message before its user image block.
        if !lifted_assistant_images || !item.parts.is_empty() {
            transcript.push(item);
        }
        if outstanding.is_empty() && !images.is_empty() {
            let mut attachment = agentkit_core::Item::new(
                agentkit_core::ItemKind::User,
                std::mem::take(&mut images),
            );
            attachment
                .metadata
                .insert("kit.projected_tool_images".into(), Value::Bool(true));
            transcript.push(attachment);
        }
    }
    if !images.is_empty() {
        return Err(LoopError::InvalidState(
            "selected-images-not-delivered: cannot attach images before all outstanding tool calls are answered. The program may already have completed; do not retry or rerun the program.".into(),
        ));
    }
    request.transcript = transcript;
    Ok(request)
}

// maybe_convert_detached adds no dedicated metadata marker. Match its exact
// result envelope only inside its Background tool results notification, never
// reinterpret arbitrary Structured tool/user output as typed media.
fn is_detached_result(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 4
            && value["call_id"].is_string()
            && value["is_error"].is_boolean()
            && value["metadata"].is_object()
            && value["output"].is_object()
    })
}

// Walk only the serialized Parts/ToolResult domain, not arbitrary JSON values
// or byte arrays. This keeps large images out of the node budget and never
// deserializes an unbounded recursive ToolResult tree.
fn project_detached_images(
    result: &mut Value,
    images: &mut Vec<Part>,
    visited: &mut usize,
    depth: usize,
) -> Result<(), LoopError> {
    if depth >= 64 {
        return Err(tool_image_traversal_error());
    }
    let call_id = result["call_id"].as_str().unwrap_or_default().to_owned();
    let Some(parts) = result["output"]
        .get_mut("Parts")
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };
    let mut previous_text = None;
    for part in parts {
        *visited += 1;
        if *visited > 100_000 {
            return Err(tool_image_traversal_error());
        }
        if part
            .get("Media")
            .is_some_and(|media| media["modality"] == "Image")
        {
            let label = format!(
                "Image from background tool call {call_id}: see the user image message immediately after this complete result batch."
            );
            let placeholder =
                serde_json::to_value(Part::text(&label)).map_err(tool_image_projection_error)?;
            let image = std::mem::replace(part, placeholder);
            let image: Part = serde_json::from_value(image).map_err(|error| LoopError::InvalidState(format!(
                "selected-images-not-delivered: invalid detached image: {error}. Do not retry or rerun the program."
            )))?;
            images.push(Part::text(label));
            if let Some(text) = previous_text.take() {
                images.push(Part::Text(text));
            }
            images.push(image);
        } else if let Some(nested) = part.get_mut("ToolResult") {
            project_detached_images(nested, images, visited, depth + 1)?;
            previous_text = None;
        } else {
            previous_text = part
                .get("Text")
                .and_then(|text| serde_json::from_value(text.clone()).ok());
        }
    }
    Ok(())
}

// Recursion is safe after the complete request passes the depth/node preflight.
fn project_result_images(
    result: &mut agentkit_core::ToolResultPart,
    images: &mut Vec<Part>,
    native: bool,
) -> Result<(), LoopError> {
    let ToolOutput::Parts(parts) = &mut result.output else {
        return Ok(());
    };
    let mut previous_text: Option<&agentkit_core::TextPart> = None;
    for part in parts.iter_mut() {
        match part {
            Part::Media(media) if !native && media.modality == Modality::Image => {
                let label = format!(
                    "Image from tool call {}: see the user image message immediately after this complete tool-result batch.",
                    result.call_id.0
                );
                images.push(Part::text(&label));
                if let Some(text) = previous_text.take() {
                    images.push(Part::Text(text.clone()));
                }
                images.push(std::mem::replace(part, Part::text(label)));
            }
            Part::ToolResult(nested) => {
                project_result_images(nested, images, native)?;
                previous_text = None;
            }
            Part::Text(text) => previous_text = Some(text),
            _ => previous_text = None,
        }
    }
    if !parts.iter().any(|part| matches!(part, Part::ToolResult(_))) {
        return Ok(());
    }
    let mut flat = Vec::new();
    for part in std::mem::take(parts) {
        if let Part::ToolResult(nested) = part {
            // Nested calls are content, not new protocol calls. Preserve their
            // provenance and diagnostics while exposing supported content parts.
            let provenance = Value::Object(serde_json::Map::from_iter([
                ("call_id".into(), Value::String(nested.call_id.0)),
                ("is_error".into(), Value::Bool(nested.is_error)),
                (
                    "metadata".into(),
                    serde_json::to_value(nested.metadata).map_err(tool_image_projection_error)?,
                ),
            ]));
            flat.push(Part::structured(Value::Object(serde_json::Map::from_iter(
                [("nested_tool_result".into(), provenance)],
            ))));
            match nested.output {
                ToolOutput::Parts(parts) => flat.extend(parts),
                ToolOutput::Text(text) => flat.push(Part::text(text)),
                ToolOutput::Structured(value) => flat.push(Part::structured(value)),
                ToolOutput::Files(files) => flat.push(Part::structured(Value::Object(
                    serde_json::Map::from_iter([(
                        "files".into(),
                        serde_json::to_value(files).map_err(tool_image_projection_error)?,
                    )]),
                ))),
            }
        } else {
            flat.push(part);
        }
    }
    *parts = flat;
    Ok(())
}

fn tool_image_projection_error(error: serde_json::Error) -> LoopError {
    LoopError::InvalidState(format!(
        "selected-images-not-delivered: image request projection failed: {error}. The program may already have completed; do not retry or rerun the program."
    ))
}

fn tool_image_traversal_error() -> LoopError {
    LoopError::InvalidState(
        "selected-images-not-delivered: tool-output image validation exceeds its traversal budget. The program may already have completed; do not retry or rerun the program.".into(),
    )
}

#[async_trait]
impl ModelSession for KitSession {
    type Turn = KitTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        let request = if matches!(self, Self::OpenAiSubscription(_)) {
            request
        } else {
            project_tool_output_images(request, false)?
        };
        if let Self::OpenRouter(session) = self
            && let Some(capability) = &session.native
            && !capability.image_input
            && request
                .transcript
                .iter()
                .flat_map(|item| &item.parts)
                .any(|part| matches!(part, Part::Media(media) if media.modality == Modality::Image))
        {
            return Err(LoopError::Provider("selected-images-not-delivered: selected OpenRouter generation model does not support image input".into()));
        }
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
                    native: session.native.is_some(),
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
                    native: false,
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
    native: bool,
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
                if turn.native {
                    normalize_native_event(&mut event)?;
                } else if let Some(ModelTurnEvent::Delta(delta)) = &mut event {
                    rewrite_openrouter_media(delta, &mut turn.media_part, &mut turn.next_media);
                }
                if let Some(context_window) = turn.context_window {
                    stamp_context_window(&mut event, context_window, "openrouter.context_length");
                }
                Ok(event)
            }
        }
    }
}

// Native image generation is opt-in through exact catalogue model selection, not
// model-name heuristics. Official capabilities never apply to custom endpoints.
#[derive(Clone)]
struct NativeImageCapability {
    image_input: bool,
    modalities: Vec<String>,
}

struct OpenRouterModelInfo {
    context_window: Option<u64>,
    input: Vec<String>,
    output: Vec<String>,
    tools: bool,
}

fn parse_openrouter_model(value: &Value, model: &str) -> Option<OpenRouterModelInfo> {
    let models = value.get("data")?.as_array()?;
    if models.len() > MAX_MODELS {
        return None;
    }
    let entry = models
        .iter()
        .find(|entry| entry["id"].as_str() == Some(model))?;
    Some(OpenRouterModelInfo {
        context_window: parse_context_window(value, model),
        input: capability_strings(&entry["architecture"]["input_modalities"]).unwrap_or_default(),
        output: capability_strings(&entry["architecture"]["output_modalities"]).unwrap_or_default(),
        tools: capability_strings(&entry["supported_parameters"])
            .unwrap_or_default()
            .iter()
            .any(|value| value == "tools"),
    })
}

const MAX_NATIVE_ENDPOINTS: usize = 256;
const MAX_CAPABILITY_VALUES: usize = 128;

fn capability_strings(value: &Value) -> Result<Vec<String>, String> {
    let values = value.as_array().ok_or("capability list must be an array")?;
    if values.len() > MAX_CAPABILITY_VALUES {
        return Err("capability list exceeds 128 entries".into());
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 128)
                .map(str::to_owned)
                .ok_or_else(|| "invalid capability list entry".into())
        })
        .collect()
}

fn native_endpoints_url(model: &str) -> Result<url::Url, LoopError> {
    let error =
        || LoopError::Provider("native-image-discovery: invalid selected model path".into());
    if !valid_model_id(model)
        || !model.contains('/')
        || model
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(error());
    }
    let mut url = url::Url::parse(OPENROUTER_MODELS_URL).map_err(|_| error())?;
    {
        let mut path = url.path_segments_mut().map_err(|_| error())?;
        for segment in model.split('/') {
            path.push(segment);
        }
        path.push("endpoints");
    }
    Ok(url)
}

async fn discover_native_image(
    client: &agentkit_http::Http,
    config: &OpenRouterConfig,
    catalog: Option<&OpenRouterModelInfo>,
) -> Result<Option<NativeImageCapability>, LoopError> {
    if !equivalent_openrouter_base_urls(&config.base_url, &OpenRouterConfig::new("", "").base_url) {
        return Ok(None);
    }
    let Some(catalog) = catalog.filter(|info| info.output.iter().any(|value| value == "image"))
    else {
        return Ok(None);
    };
    let url = native_endpoints_url(&config.model)?;
    let response = client.get(url.as_str()).send().await.map_err(|_| {
        LoopError::Provider(
            "native-image-discovery: endpoint transport failed; generation eligibility is unknown"
                .into(),
        )
    })?;
    if !response.status().is_success() {
        return Err(LoopError::Provider(format!(
            "native-image-discovery: endpoint catalog returned {}; generation eligibility is unknown",
            response.status()
        )));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            LoopError::Provider("native-image-discovery: endpoint body failed".into())
        })?;
        if chunk.len() > MAX_MODELS_BYTES.saturating_sub(body.len()) {
            return Err(LoopError::Provider(
                "native-image-discovery: endpoint catalog exceeds 2 MiB".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&body).map_err(|_| {
        LoopError::Provider("native-image-discovery: endpoint catalog is not valid JSON".into())
    })?;
    endpoint_image_capability(config, catalog, &value)
}

fn endpoint_image_capability(
    config: &OpenRouterConfig,
    catalog: &OpenRouterModelInfo,
    value: &Value,
) -> Result<Option<NativeImageCapability>, LoopError> {
    let invalid = |reason: &str| {
        LoopError::Provider(format!(
            "native-image-discovery: {reason}; generation eligibility is unknown"
        ))
    };
    let data = &value["data"];
    if data["id"].as_str() != Some(config.model.as_str()) {
        return Err(invalid("endpoint model identity mismatch"));
    }
    let input = capability_strings(&data["architecture"]["input_modalities"])
        .map_err(|error| invalid(&error))?;
    let output = capability_strings(&data["architecture"]["output_modalities"])
        .map_err(|error| invalid(&error))?;
    let same_values = |a: &[String], b: &[String]| {
        a.len() == b.len()
            && a.iter().all(|value| b.contains(value))
            && b.iter().all(|value| a.contains(value))
    };
    if !same_values(&input, &catalog.input) || !same_values(&output, &catalog.output) {
        return Err(invalid(
            "endpoint architecture disagrees with model catalog",
        ));
    }
    let endpoints = data["endpoints"]
        .as_array()
        .ok_or_else(|| invalid("missing endpoint list"))?;
    if endpoints.len() > MAX_NATIVE_ENDPOINTS {
        return Err(invalid("endpoint count exceeds 256"));
    }
    // Routing selectors can advertise broad aggregate modalities without having
    // concrete generation providers. An empty list preserves legacy routing.
    if endpoints.is_empty() {
        return Ok(None);
    }
    let mut tools = false;
    for endpoint in endpoints {
        let parameters = capability_strings(&endpoint["supported_parameters"])
            .map_err(|error| invalid(&error))?;
        tools |= parameters.iter().any(|parameter| parameter == "tools");
    }
    if !tools {
        return Err(LoopError::Provider("native-image-ineligible: no concrete OpenRouter endpoint supports tools; compose cannot be removed".into()));
    }
    let concrete = OpenRouterModelInfo {
        context_window: catalog.context_window,
        input,
        output,
        tools: catalog.tools && tools,
    };
    native_image_capability(config, Some(&concrete))
}

fn native_image_capability(
    config: &OpenRouterConfig,
    info: Option<&OpenRouterModelInfo>,
) -> Result<Option<NativeImageCapability>, LoopError> {
    if !equivalent_openrouter_base_urls(&config.base_url, &OpenRouterConfig::new("", "").base_url) {
        return Ok(None);
    }
    let Some(info) = info.filter(|info| info.output.iter().any(|value| value == "image")) else {
        return Ok(None);
    };
    if !info.tools {
        return Err(LoopError::Provider("OpenRouter image model is not eligible for a Kit agent: catalogue does not advertise tools support; compose cannot be removed".into()));
    }
    if info
        .output
        .iter()
        .any(|value| value != "image" && value != "text")
    {
        return Err(LoopError::Provider(
            "OpenRouter image model advertises unsupported output modalities".into(),
        ));
    }
    Ok(Some(NativeImageCapability {
        image_input: info.input.iter().any(|value| value == "image"),
        modalities: info.output.clone(),
    }))
}

fn native_generation_config(
    mut config: OpenRouterConfig,
    capability: &NativeImageCapability,
) -> Result<OpenRouterConfig, LoopError> {
    let routing = config
        .extra_body
        .entry("provider".into())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let routing = routing.as_object_mut().ok_or_else(|| {
        LoopError::Provider(
            "native-image-configuration: provider routing options must be an object".into(),
        )
    })?;
    routing.insert("require_parameters".into(), Value::Bool(true));
    // The proven generation contract is a complete JSON response. Buffer only
    // after the transport enforces its raw-byte ceiling, before the decoder.
    Ok(config.with_streaming(false).with_extra_body_value(
        "modalities",
        Value::Array(
            capability
                .modalities
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    ))
}

const NATIVE_GENERATION_TIMEOUT: Duration = Duration::from_secs(300);

fn native_generation_resilience() -> agentkit_http::ResilienceConfig {
    // A timed-out/nonstreaming generation may already have been billed. Keep
    // authentication and cancellation under a finite logical deadline, but do
    // not automatically replay ambiguous generation failures or HTTP statuses.
    agentkit_http::ResilienceConfig {
        max_retries: 0,
        retry_budget: Duration::from_secs(310),
        attempt_timeout: Some(NATIVE_GENERATION_TIMEOUT),
        stream_idle_timeout: Some(NATIVE_GENERATION_TIMEOUT),
        ..agentkit_http::ResilienceConfig::default()
    }
}

const MAX_NATIVE_RESPONSE_BYTES: usize = 24 * 1024 * 1024;
const MAX_NATIVE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_NATIVE_DELIVERY_BYTES: usize = 16 * 1024 * 1024;

struct BoundedImageClient {
    inner: agentkit_http::Http,
}

#[async_trait]
impl agentkit_http::HttpClient for BoundedImageClient {
    async fn execute(
        &self,
        request: agentkit_http::HttpRequest,
    ) -> Result<agentkit_http::HttpResponse, agentkit_http::HttpError> {
        use agentkit_http::{HttpError, HttpResponse};
        let response = self.inner.execute(request).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let url = response.url().to_owned();
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if chunk.len() > MAX_NATIVE_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(HttpError::Other(
                    "native-image-response-too-large: raw response exceeds 24 MiB".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if status.is_success() {
            let value: Value = serde_json::from_slice(&body).map_err(|_| {
                HttpError::Other("native-image-malformed: expected a complete JSON response".into())
            })?;
            // The pinned decoder skips malformed image entries. Reject these
            // before decoding so a valid sibling cannot mask invalid media.
            let choices = value["choices"].as_array().ok_or_else(|| {
                HttpError::Other("native-image-malformed: missing choices".into())
            })?;
            if choices.len() != 1 {
                return Err(HttpError::Other(
                    "native-image-malformed: expected exactly one completion choice".into(),
                ));
            }
            let mut image_count = 0_usize;
            let mut encoded_bytes = 0_usize;
            for choice in choices {
                for key in ["message", "delta"] {
                    let message = &choice[key];
                    let images = match message.get("images") {
                        Some(images) => images
                            .as_array()
                            .ok_or_else(|| {
                                HttpError::Other(
                                    "native-image-malformed: images must be an array".into(),
                                )
                            })?
                            .as_slice(),
                        None => &[],
                    };
                    // Both native message.images and standard content image_url
                    // parts pass the same strict checks, including skipped entries.
                    let content_images = message["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|part| part["type"] == "image_url");
                    for image in images.iter().chain(content_images) {
                        let uri = image["image_url"]["url"].as_str().ok_or_else(|| {
                            HttpError::Other("native-image-malformed: missing image URL".into())
                        })?;
                        let (_, payload) = native_image_payload(uri)
                            .map_err(|error| HttpError::Other(error.to_string()))?;
                        image_count += 1;
                        encoded_bytes += payload.len();
                        if image_count > 8
                            || encoded_bytes > MAX_NATIVE_DELIVERY_BYTES.div_ceil(3) * 4 + 8 * 4
                        {
                            return Err(HttpError::Other(
                                "native-image-too-large: aggregate images exceed delivery budget"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }
        Ok(HttpResponse::new(
            status,
            headers,
            url,
            Box::pin(futures_util::stream::once(async move {
                Ok(bytes::Bytes::from(body))
            })),
        ))
    }
}

fn native_image_payload(uri: &str) -> Result<(&str, &str), LoopError> {
    let malformed = || {
        LoopError::Provider("native-image-malformed: expected inline base64 PNG or JPEG; remote and file URIs are not fetched".into())
    };
    let (header, payload) = uri.split_once(',').ok_or_else(malformed)?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .ok_or_else(malformed)?;
    if !matches!(mime, "image/png" | "image/jpeg" | "image/*") || payload.is_empty() {
        return Err(malformed());
    }
    if payload.len() > MAX_NATIVE_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(LoopError::Provider(
            "native-image-too-large: encoded image exceeds 8 MiB decoded budget".into(),
        ));
    }
    Ok((mime, payload))
}

fn normalize_native_part(part: &mut Part) -> Result<(usize, u64), LoopError> {
    use base64::Engine as _;
    let Part::Media(media) = part else {
        return Ok((0, 0));
    };
    if media.modality != Modality::Image {
        return Err(LoopError::Provider(
            "native-image-malformed: unsupported media modality".into(),
        ));
    }
    let DataRef::Uri(uri) = &media.data else {
        return Err(LoopError::Provider(
            "native-image-malformed: expected inline image URI".into(),
        ));
    };
    let (declared, payload) = native_image_payload(uri)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| LoopError::Provider("native-image-malformed: invalid base64".into()))?;
    if bytes.len() > MAX_NATIVE_IMAGE_BYTES {
        return Err(LoopError::Provider(
            "native-image-too-large: decoded image exceeds 8 MiB".into(),
        ));
    }
    let (mime, pixels) =
        crate::managed_files::validate_provider_image(&bytes).map_err(|error| {
            // The shared validator returns strings; preserve the bounded decoder's
            // size failures separately from invalid encoding/format failures.
            let code = match error.as_str() {
                "image exceeds decoded pixel or allocation budget"
                | "Memory limit exceeded"
                | "Image size exceeds limit" => "native-image-too-large",
                _ => "native-image-malformed",
            };
            LoopError::Provider(format!("{code}: {error}"))
        })?;
    if declared != "image/*" && declared != mime {
        return Err(LoopError::Provider(
            "native-image-malformed: declared MIME disagrees with image bytes".into(),
        ));
    }
    let size = bytes.len();
    media.mime_type = mime;
    media.data = DataRef::InlineBytes(bytes);
    Ok((size, pixels))
}

fn normalize_native_event(event: &mut Option<ModelTurnEvent>) -> Result<(), LoopError> {
    match event {
        Some(ModelTurnEvent::Delta(Delta::CommitPart { part })) => {
            normalize_native_part(part)?;
        }
        Some(ModelTurnEvent::Finished(result)) => {
            let mut count = 0;
            let mut bytes = 0;
            let mut pixels = 0;
            for part in result
                .output_items
                .iter_mut()
                .flat_map(|item| &mut item.parts)
            {
                let (size, image_pixels) = normalize_native_part(part)?;
                count += usize::from(size > 0);
                bytes += size;
                pixels += image_pixels;
                if count > 8 || bytes > MAX_NATIVE_DELIVERY_BYTES || pixels > 32 * 1024 * 1024 {
                    return Err(LoopError::Provider("native-image-too-large: delivery exceeds 8 images, 16 MiB or 32 megapixels".into()));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
#[path = "adapter_native_tests.rs"]
mod native_tests;

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

pub(super) fn stamp_context_window(
    event: &mut Option<ModelTurnEvent>,
    context_window: u64,
    provider_key: &'static str,
) {
    let stamp = |usage: &mut Usage| {
        usage
            .metadata
            .insert("context_window".into(), context_window.into());
        usage
            .metadata
            .insert(provider_key.into(), context_window.into());
    };
    match event {
        Some(ModelTurnEvent::Usage(usage)) => stamp(usage),
        Some(ModelTurnEvent::Finished(result)) => {
            if let Some(usage) = &mut result.usage {
                stamp(usage);
            }
            for item in &mut result.output_items {
                if let Some(usage) = &mut item.usage {
                    stamp(usage);
                }
            }
        }
        _ => {}
    }
}

fn models_url(completions_url: &str) -> Option<String> {
    completions_url
        .trim_end_matches('/')
        .strip_suffix("/chat/completions")
        .map(|prefix| format!("{prefix}/models"))
}

async fn fetch_openrouter_model(
    client: &reqwest::Client,
    url: &str,
    model: &str,
) -> Result<OpenRouterModelInfo, String> {
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
    parse_openrouter_model(&value, model)
        .ok_or_else(|| "OpenRouter model catalog omitted selected model".to_owned())
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

const OPENAI_FALLBACK: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];

fn openai_models(discovered: Vec<String>, current: &ModelSelection) -> Vec<String> {
    let mut models = if discovered.is_empty() {
        OPENAI_FALLBACK
            .iter()
            .map(|model| (*model).to_string())
            .collect::<Vec<_>>()
    } else {
        discovered
    };
    let mut seen = std::collections::HashSet::new();
    models.retain(|model| seen.insert(model.clone()));
    if current.provider == ProviderKind::OpenAiSubscription
        && valid_model_id(&current.model)
        && !models.contains(&current.model)
    {
        models.push(current.model.clone());
    }
    models
}

/// Returns a bounded, provider-grouped catalog. Remote discovery is best effort.
pub async fn model_catalog(current: &ModelSelection) -> Vec<ModelGroup> {
    match SelectableAdapter::new(current.provider, current.model.clone()) {
        Ok(adapter) => adapter.model_catalog(current).await,
        Err(_) => {
            model_catalog_with_openai(current, std::future::ready(DiscoveredModels::default()))
                .await
        }
    }
}

async fn model_catalog_with_openai(
    current: &ModelSelection,
    openai_catalog: impl std::future::Future<Output = DiscoveredModels>,
) -> Vec<ModelGroup> {
    let openrouter_url = match std::env::var_os("OPENROUTER_BASE_URL") {
        None => catalog_models_url(None),
        Some(url) => url.to_str().and_then(|url| catalog_models_url(Some(url))),
    };
    let public_url = OPENROUTER_MODELS_URL.to_string();
    let same_catalog = openrouter_url.as_deref() == Some(public_url.as_str());
    let other_catalogs = async move {
        if same_catalog {
            let models = fetch_model_ids(&public_url)
                .await
                .unwrap_or_else(|_| fallback_catalog());
            (models.clone(), models)
        } else {
            let openrouter_catalog = async {
                match openrouter_url {
                    Some(url) => fetch_model_ids(&url)
                        .await
                        .unwrap_or_else(|_| fallback_catalog()),
                    None => fallback_catalog(),
                }
            };
            let speakeasy_catalog = async {
                fetch_model_ids(&public_url)
                    .await
                    .unwrap_or_else(|_| fallback_catalog())
            };
            futures_util::future::join(openrouter_catalog, speakeasy_catalog).await
        }
    };
    let (discovered_openai, (openrouter, speakeasy)) =
        futures_util::future::join(openai_catalog, other_catalogs).await;
    let openai_windows = discovered_openai.context_windows;
    let openrouter_windows = openrouter.context_windows;
    let speakeasy_windows = speakeasy.context_windows;
    let mut openrouter = openrouter.models;
    let mut speakeasy = speakeasy.models;
    let openai = openai_models(discovered_openai.models, current);
    let current_is_valid = valid_model_id(&current.model);
    if current_is_valid
        && current.provider == ProviderKind::OpenRouter
        && !openrouter.contains(&current.model)
    {
        openrouter.push(current.model.clone());
    }
    if current_is_valid
        && current.provider == ProviderKind::Speakeasy
        && !speakeasy.contains(&current.model)
    {
        speakeasy.push(current.model.clone());
    }
    openrouter.sort();
    openrouter.dedup();
    openrouter.truncate(MAX_SELECTOR_MODELS);
    if current_is_valid
        && current.provider == ProviderKind::OpenRouter
        && !openrouter.contains(&current.model)
    {
        if openrouter.len() == MAX_SELECTOR_MODELS {
            openrouter.pop();
        }
        openrouter.push(current.model.clone());
    }
    speakeasy.sort();
    speakeasy.dedup();
    speakeasy.truncate(MAX_SELECTOR_MODELS);
    if current_is_valid
        && current.provider == ProviderKind::Speakeasy
        && !speakeasy.contains(&current.model)
    {
        if speakeasy.len() == MAX_SELECTOR_MODELS {
            speakeasy.pop();
        }
        speakeasy.push(current.model.clone());
    }
    vec![
        ModelGroup {
            provider: ProviderKind::OpenAiSubscription,
            models: openai,
            context_windows: openai_windows,
        },
        ModelGroup {
            provider: ProviderKind::OpenRouter,
            models: openrouter,
            context_windows: openrouter_windows,
        },
        ModelGroup {
            provider: ProviderKind::Speakeasy,
            models: speakeasy,
            context_windows: speakeasy_windows,
        },
    ]
}

fn openrouter_fallback() -> Vec<String> {
    OPENROUTER_FALLBACK
        .iter()
        .map(|model| (*model).to_string())
        .collect()
}

#[derive(Clone, Default)]
struct DiscoveredModels {
    models: Vec<String>,
    context_windows: std::collections::HashMap<String, u64>,
}

fn fallback_catalog() -> DiscoveredModels {
    DiscoveredModels {
        models: openrouter_fallback(),
        ..Default::default()
    }
}

async fn fetch_model_ids(url: &str) -> Result<DiscoveredModels, String> {
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
    parse_discovered_models(&value)
}

fn parse_discovered_models(value: &Value) -> Result<DiscoveredModels, String> {
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "model catalog omitted data".to_string())?;
    if entries.len() > MAX_MODELS {
        return Err("model catalog has too many entries".into());
    }
    let models: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .filter(|id| valid_model_id(id))
        .take(MAX_SELECTOR_MODELS)
        .map(str::to_string)
        .collect();
    let context_windows = models
        .iter()
        .filter_map(|id| parse_context_window(value, id).map(|window| (id.clone(), window)))
        .collect();
    Ok(DiscoveredModels {
        models,
        context_windows,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    #[test]
    fn model_switch_catalog_retains_only_reported_positive_windows() {
        let catalog = super::parse_discovered_models(&serde_json::json!({"data": [
            {"id": "known", "context_length": 200000},
            {"id": "unknown"}, {"id": "zero", "context_length": 0},
            {"id": "bad model", "context_length": 100}
        ]}))
        .unwrap();
        assert_eq!(catalog.models, ["known", "unknown", "zero"]);
        assert_eq!(catalog.context_windows.len(), 1);
        assert_eq!(catalog.context_windows["known"], 200000);
        assert!(super::fallback_catalog().context_windows.is_empty());
    }

    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    use agentkit_core::{
        DataRef, Delta, FinishReason, Item, ItemKind, MediaPart, MetadataMap, Modality, Part,
        PartId, PartKind, SessionId, TokenUsage, ToolCallId, ToolOutput, ToolResultPart, TurnId,
        Usage,
    };
    use agentkit_loop::{
        LoopError, ModelAdapter, ModelSession, ModelTurnEvent, ModelTurnResult, SessionConfig,
        TurnRequest,
    };
    use agentkit_provider_openrouter::{
        OpenRouterAdapter, OpenRouterConfig, ReasoningEffort as OpenRouterReasoningEffort,
    };
    use serde_json::json;

    use crate::credentials::CredentialStorage;

    use super::{
        KitAdapter, KitSession, ModelSelection, OPENAI_FALLBACK, OPENROUTER_MODELS_URL,
        OpenRouterApiKey, OpenRouterKitSession, OpenRouterProvider, ProviderKind, ReasoningEffort,
        SelectableAdapter, SelectableSession, SessionSelection, SpeakeasyKitAdapter,
        SpeakeasyProvider, apply_openrouter_reasoning_effort, catalog_models_url,
        expose_background_call_ids, gram_chat_id, models_url, openai_models,
        openrouter_config_from_env, parse_context_window, rewrite_openrouter_media,
        stamp_context_window,
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

    async fn assert_openrouter_bearer(config: &OpenRouterConfig, expected: &str) {
        let attempt = config.authentication.authenticate(None).await.unwrap();
        assert_eq!(
            attempt.headers()["authorization"].to_str().unwrap(),
            format!("Bearer {expected}"),
        );
    }

    #[tokio::test]
    async fn openrouter_key_source_controls_custom_base_url_access() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().join("credentials"));
        crate::provider::store_openrouter_test_credentials(&storage);

        let config = openrouter_config_from_env("selected/model".into(), &storage, None, |_| {
            Err(std::env::VarError::NotPresent)
        })
        .unwrap();
        assert_openrouter_bearer(&config, "test-openrouter-key").await;
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

        assert_openrouter_bearer(&config, "test-openrouter-key").await;
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
        assert_openrouter_bearer(&config, "environment-key").await;
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
        assert_openrouter_bearer(&config, "explicit-key").await;
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
    fn selectable_adapter_defers_openrouter_credentials_until_session_start() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().join("credentials"));

        SelectableAdapter::new_with_credentials_effort_and_openrouter_key(
            ProviderKind::OpenRouter,
            "test/model",
            storage,
            None,
            None,
        )
        .unwrap();
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
    fn detached_results_tell_the_model_their_call_id_and_remind_it_to_stop() {
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
            "Tool call ID: call_background is running in the background.\nNo independent work left? STOP."
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
        let adapter = KitAdapter::Speakeasy(Box::new(SpeakeasyKitAdapter {
            provider,
            client: agentkit_http::Http::new(client),
        }));
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
            native: None,
        })
    }

    fn selectable_session(active: ModelSelection, inner: KitSession) -> SelectableSession {
        let active = SessionSelection {
            model: active,
            reasoning_effort: None,
            revision: 0,
        };
        SelectableSession {
            selection: Arc::new(Mutex::new(active.clone())),
            credential_storage: Default::default(),
            openrouter_api_key: None,
            openai_model_catalog: Default::default(),
            config: SessionConfig::new("provider-identity-test"),
            active,
            inner,
        }
    }

    fn selected_image_request(nested: bool) -> TurnRequest {
        let image = Part::media(
            Modality::Image,
            "image/png",
            DataRef::InlineBytes(vec![1, 2, 3]),
        );
        let output = if nested {
            vec![Part::ToolResult(ToolResultPart::success(
                "nested",
                ToolOutput::Parts(vec![image]),
            ))]
        } else {
            vec![Part::text("Selected image"), image]
        };
        TurnRequest {
            session_id: SessionId::new("provider-identity-test"),
            turn_id: TurnId::new("replay"),
            transcript: vec![
                Item::new(
                    ItemKind::Tool,
                    vec![Part::ToolResult(ToolResultPart::success(
                        "completed-call",
                        ToolOutput::Parts(output),
                    ))],
                ),
                Item::text(ItemKind::User, "Continue after switching providers"),
            ],
            available_tools: Vec::new(),
            cache: None,
            metadata: MetadataMap::new(),
        }
    }

    #[test]
    fn selected_image_projection_bounds_nested_and_wide_outputs() {
        let mut request = selected_image_request(false);
        let mut part = Part::text("deep");
        for _ in 0..65 {
            part = Part::ToolResult(ToolResultPart::success(
                "nested",
                ToolOutput::Parts(vec![part]),
            ));
        }
        request.transcript[0].parts = vec![part];
        let error = super::project_tool_output_images(request.clone(), false).unwrap_err();
        assert!(error.to_string().contains("traversal budget"));
        request.transcript[0].parts = vec![Part::ToolResult(ToolResultPart::success(
            "wide",
            ToolOutput::Parts(vec![Part::text("text"); 100_001]),
        ))];
        let error = super::project_tool_output_images(request.clone(), false).unwrap_err();
        assert!(error.to_string().contains("do not retry or rerun"));
    }

    #[test]
    fn selected_image_projection_preserves_user_images_and_text_tool_outputs() {
        let mut request = selected_image_request(false);
        request.transcript[0] = Item::new(
            ItemKind::User,
            vec![Part::media(
                Modality::Image,
                "image/png",
                DataRef::InlineBytes(vec![1, 2, 3]),
            )],
        );
        for output in [
            ToolOutput::Text("done".into()),
            ToolOutput::Structured(json!({"ok": true})),
            ToolOutput::Parts(vec![Part::text("done")]),
        ] {
            request.transcript.push(Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(ToolResultPart::success(
                    "text-call",
                    output,
                ))],
            ));
        }
        let original = serde_json::to_value(&request.transcript).unwrap();
        for native in [false, true] {
            let projected = super::project_tool_output_images(request.clone(), native).unwrap();
            assert_eq!(
                serde_json::to_value(&projected.transcript).unwrap(),
                original
            );
        }
        assert_eq!(serde_json::to_value(&request.transcript).unwrap(), original);
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
                    revision: 1,
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
                        revision: 1,
                    },
                    Err(LoopError::Provider("replacement failed".into())),
                )
                .is_err()
        );

        assert_eq!(session.provider_name(), Some("openrouter"));
        assert_eq!(session.model_name(), Some("test/initial"));
    }

    #[test]
    fn openai_catalog_keeps_the_active_custom_model() {
        let current = ModelSelection::new(ProviderKind::OpenAiSubscription, "custom-model");

        assert!(openai_models(Vec::new(), &current).contains(&current.model));
    }

    #[test]
    fn openai_catalog_prefers_discovery_and_preserves_order() {
        let current = ModelSelection::new(ProviderKind::OpenAiSubscription, "custom-model");
        let models = openai_models(
            vec!["second".into(), "first".into(), "second".into()],
            &current,
        );

        assert_eq!(models, ["second", "first", "custom-model"]);
    }

    #[test]
    fn openai_catalog_uses_fallback_only_when_discovery_is_empty() {
        let current = ModelSelection::new(ProviderKind::OpenRouter, "other/model");

        assert_eq!(openai_models(Vec::new(), &current), OPENAI_FALLBACK);
    }

    #[test]
    fn reselecting_the_active_model_refreshes_the_next_turn() {
        let credentials = crate::credentials::CredentialStorage::Memory;
        crate::provider::store_openrouter_test_credentials(&credentials);
        let adapter = SelectableAdapter::new_with_credentials(
            ProviderKind::OpenRouter,
            "test-model",
            credentials,
        )
        .unwrap();
        let before = adapter.selection.lock().unwrap().clone();

        adapter.select(before.model.clone()).unwrap();

        let after = adapter.selection.lock().unwrap().clone();
        assert_eq!(after.model, before.model);
        assert_eq!(after.reasoning_effort, before.reasoning_effort);
        assert_ne!(after.revision, before.revision);
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
                    "not supported",
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
    fn context_window_stamping_traverses_finished_usage() {
        let mut item = Item::text(ItemKind::Assistant, "done");
        item.usage = Some(Usage::new(TokenUsage::default()));
        let mut event = Some(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![item],
            usage: Some(Usage::new(TokenUsage::default())),
            metadata: MetadataMap::new(),
            model: None,
            response_id: None,
        }));

        stamp_context_window(&mut event, 200_000, "openrouter.context_length");

        let Some(ModelTurnEvent::Finished(result)) = event else {
            panic!("expected finished event");
        };
        for usage in [
            result.usage.as_ref().unwrap(),
            result.output_items[0].usage.as_ref().unwrap(),
        ] {
            assert_eq!(usage.metadata["context_window"], json!(200_000));
            assert_eq!(usage.metadata["openrouter.context_length"], json!(200_000));
        }
    }
}

#[cfg(test)]
#[path = "adapter_image_tests.rs"]
mod image_tests;

#[cfg(test)]
#[path = "adapter_background_image_tests.rs"]
mod background_image_tests;
