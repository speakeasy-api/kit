use std::{sync::Arc, time::Duration};

use agentkit_core::{TurnCancellation, Usage};
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

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiSubscription => "openai-subscription",
            Self::OpenRouter => "openrouter",
        }
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
        match provider {
            ProviderKind::OpenAiSubscription => {
                OpenAiSubscriptionAdapter::new(SubscriptionConfig::new(model)?)
                    .map(Self::OpenAiSubscription)
            }
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
}

pub enum KitTurn {
    OpenAiSubscription(Box<OpenAiSubscriptionTurn>),
    OpenRouter(OpenRouterKitTurn),
}

pub struct OpenRouterKitTurn {
    inner: OpenRouterTurn,
    context_window: Option<u64>,
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

#[cfg(test)]
mod tests {
    use agentkit_core::{TokenUsage, Usage};
    use serde_json::json;

    use super::{add_context_window, models_url, parse_context_window};

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
