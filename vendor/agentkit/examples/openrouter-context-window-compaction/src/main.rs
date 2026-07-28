//! Demonstrates a context-window-aware compaction trigger driven by
//! `Usage` reports from the OpenRouter adapter.
//!
//! - `agentkit_compaction::context_window_trigger` fires when the latest
//!   transcript item's reported `input_tokens` crosses
//!   `context_length * percentage / 100`.
//! - `agentkit_compaction::AgentCompactor` runs a nested loop over the same
//!   adapter to summarise older items into a single `Context` item.
//! - The model's `context_length` is fetched from OpenRouter's
//!   `/api/v1/models` catalog at startup, so the threshold matches the
//!   pinned model rather than a hardcoded value.

use std::env;
use std::error::Error;
use std::sync::Arc;

use agentkit_compaction::{
    AgentBuilderCompactorExt, AgentCompactor, CompactionPipeline, DropFailedToolResultsStrategy,
    DropReasoningStrategy, StrategyCompactor, SummarizeOlderStrategy, context_window_trigger,
};
use agentkit_core::{Item, ItemKind, Part, SessionId};
use agentkit_http::Http;
use agentkit_loop::{
    Agent, AgentEvent, InputRequest, LoopInterrupt, LoopObserver, LoopStep, ObservedEvent,
    PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tools_core::{PermissionChecker, PermissionDecision};
use serde::Deserialize;

const SYSTEM_PROMPT: &str = "\
You are a concise assistant. Answer in a single sentence under 40 words. \
If the transcript contains a compaction summary item, treat it as authoritative memory.";

const COMPACTION_SYSTEM_PROMPT: &str = "\
You are a compaction agent. Compress transcript history into a durable \
context note for another agent that has lost the original messages. \
Preserve every named person, every year and date, every place, every \
plot point, and every actionable fact. Drop chatter, narration, and \
chain-of-thought. Return only the compacted note as plain text.";

/// Embedded prose sized so that, after first-turn processing, the
/// provider-reported `input_tokens` crosses 60% of the pinned model's real
/// context window.
const STORY: &str = include_str!("story.txt");

/// Pinned to an OpenAI-routed model so `usage.prompt_tokens` is the real
/// (tiktoken) count, not a normalised billing approximation. Some
/// OpenRouter routes for free or community-hosted models cap reported
/// tokens regardless of input size, which makes a context-window trigger
/// impossible to fire reliably on those routes.
const DEFAULT_MODEL: &str = "openai/gpt-3.5-turbo";
/// Fire compaction once input tokens reach this share of the window.
const DEFAULT_PERCENTAGE: u32 = 60;

/// Prints turn, usage, compaction, and failure events to stdout/stderr.
#[derive(Clone, Default)]
struct DisplayObserver;

impl LoopObserver for DisplayObserver {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        match event {
            AgentEvent::TurnStarted { turn_id, .. } => {
                println!("[turn] {turn_id} started");
            }
            AgentEvent::UsageUpdated(usage) => {
                if let Some(tokens) = usage.tokens {
                    println!(
                        "[usage] input_tokens={} output_tokens={} cached={:?}",
                        tokens.input_tokens, tokens.output_tokens, tokens.cached_input_tokens
                    );
                } else {
                    println!("[usage] provider returned no token counts");
                }
            }
            AgentEvent::TurnFinished(result) => {
                println!(
                    "[turn] {} finished reason={:?} items={}",
                    result.turn_id,
                    result.finish_reason,
                    result.items.len(),
                );
            }
            AgentEvent::MutationStarted { mutator, point, .. } => {
                println!("[mutation] start mutator={mutator} point={point:?}");
            }
            AgentEvent::MutationFinished {
                mutator,
                dirty,
                metadata,
                ..
            } => {
                let replaced = metadata
                    .get("replaced_items")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                println!("[mutation] finished mutator={mutator} dirty={dirty} replaced={replaced}");
            }
            AgentEvent::Warning { message } => {
                eprintln!("[warning] {message}");
            }
            AgentEvent::RunFailed { message } => {
                eprintln!("[run-failed] {message}");
            }
            _ => {}
        }
    }
}

struct AllowAll;

impl PermissionChecker for AllowAll {
    fn evaluate(
        &self,
        _request: &dyn agentkit_tools_core::PermissionRequest,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

#[derive(Deserialize)]
struct EndpointsResponse {
    data: EndpointsData,
}

#[derive(Deserialize)]
struct EndpointsData {
    endpoints: Vec<EndpointInfo>,
}

#[derive(Deserialize)]
struct EndpointInfo {
    context_length: Option<u64>,
}

/// Looks up `context_length` for `model` from OpenRouter's
/// `/api/v1/models/{model}/endpoints` route. Chat completion responses
/// don't carry the window size, and the full `/v1/models` catalog is
/// hundreds of entries, so the per-model endpoints route is the cheapest
/// way to fetch it.
async fn fetch_context_length(api_key: &str, model: &str) -> Result<u64, Box<dyn Error>> {
    let http = Http::new(reqwest::Client::new());
    let url = format!("https://openrouter.ai/api/v1/models/{model}/endpoints");
    let response = http
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await?
        .error_for_status()?;
    let body: EndpointsResponse = response.json().await?;
    // OpenRouter may route the same model to multiple upstream providers
    // with different advertised windows. We don't know which one a given
    // request will land on, so take the smallest as a conservative budget.
    // (For deterministic routing, pin a provider via the `provider` field
    // in the request body and look up that endpoint specifically.)
    body.data
        .endpoints
        .iter()
        .filter_map(|e| e.context_length)
        .min()
        .ok_or_else(|| format!("no endpoint for {model} reports context_length").into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let percentage = parse_env_u32("CONTEXT_PERCENTAGE").unwrap_or(DEFAULT_PERCENTAGE);
    let api_key = env::var("OPENROUTER_API_KEY")
        .map_err(|_| "OPENROUTER_API_KEY must be set in the environment or .env")?;

    let mut config = OpenRouterConfig::from_env()?
        .with_temperature(0.0)
        .with_max_completion_tokens(512);
    let env_model = env::var("OPENROUTER_MODEL").ok();
    if let Some(envm) = env_model.as_deref()
        && envm != DEFAULT_MODEL
    {
        eprintln!(
            "[note] ignoring OPENROUTER_MODEL={envm}; this example pins {DEFAULT_MODEL} \
             (OpenAI-routed, reports native token counts)"
        );
    }
    config.model = DEFAULT_MODEL.into();
    let model = config.model.clone();
    let adapter = OpenRouterAdapter::new(config)?;

    println!("[startup] fetching context_length for {model} from OpenRouter...");
    let context_length = fetch_context_length(&api_key, &model).await?;

    let backend_agent = Arc::new(
        Agent::builder()
            .model(adapter.clone())
            .permissions(AllowAll)
            .build()?,
    );
    let backend = AgentCompactor::builder()
        .agent(backend_agent)
        .session_id(SessionId::from(
            "openrouter-context-window-compaction-compactor",
        ))
        .system_prompt(COMPACTION_SYSTEM_PROMPT)
        .build()?;

    let prompts = [
        format!(
            "Below is a short story. Read it, then summarize it in 3 to 4 sentences. \
             Preserve names, dates, and places.\n\n---\n{STORY}---"
        ),
        "Based on your summary, what year did Marcus's wife die?".to_owned(),
        "Based on your summary, who is Holger Kvist?".to_owned(),
    ];

    let compactor = StrategyCompactor::builder()
        .trigger(context_window_trigger(context_length, percentage))
        .strategy(
            CompactionPipeline::new()
                .with_strategy(DropReasoningStrategy::new())
                .with_strategy(DropFailedToolResultsStrategy::new())
                .with_strategy(
                    SummarizeOlderStrategy::new(1)
                        .preserve_kind(ItemKind::System)
                        .preserve_kind(ItemKind::Context),
                ),
        )
        .backend(backend)
        .build()?;

    let agent = Agent::builder()
        .model(adapter)
        .permissions(AllowAll)
        .compactor(compactor)
        .observer(DisplayObserver)
        .transcript(vec![Item::text(ItemKind::System, SYSTEM_PROMPT)])
        .input(vec![Item::text(ItemKind::User, &prompts[0])])
        .build()?;

    let threshold = context_length.saturating_mul(percentage as u64) / 100;
    println!(
        "[config] model={model} context_length={context_length} percentage={percentage}% threshold={threshold}"
    );

    let mut driver = agent
        .start(
            SessionConfig::new("openrouter-context-window-compaction").with_cache(
                PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
            ),
        )
        .await?;

    println!("\nyou> {}", short(&prompts[0]));
    let mut pending = drive_to_idle(&mut driver).await?;

    for prompt in prompts.iter().skip(1) {
        println!("\nyou> {prompt}");
        pending.submit(&mut driver, vec![Item::text(ItemKind::User, prompt)])?;
        pending = drive_to_idle(&mut driver).await?;
    }

    let snapshot = driver.snapshot();
    println!(
        "\n[done] transcript_len={} pending_input={}",
        snapshot.transcript.len(),
        snapshot.pending_input.len()
    );

    Ok(())
}

async fn drive_to_idle<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
) -> Result<InputRequest, Box<dyn Error>>
where
    S: agentkit_loop::ModelSession,
{
    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                if result.items.is_empty() {
                    println!("(no items returned this turn)");
                }
                for item in result.items {
                    let label = match item.kind {
                        ItemKind::Assistant => "assistant",
                        ItemKind::Tool => "tool",
                        ItemKind::System => "system",
                        ItemKind::User => "user",
                        ItemKind::Developer => "developer",
                        ItemKind::Context => "context",
                        ItemKind::Notification => "notification",
                    };
                    if item.parts.is_empty() {
                        println!("{label}> (empty)");
                        continue;
                    }
                    for part in item.parts {
                        match part {
                            Part::Text(text) => println!("{label}> {}", text.text),
                            Part::Reasoning(r) => println!(
                                "{label}> [reasoning] {}",
                                r.summary.unwrap_or_else(|| "<no summary>".into())
                            ),
                            Part::ToolCall(c) => {
                                println!("{label}> [tool call] {} {}", c.name, c.input)
                            }
                            Part::ToolResult(r) => {
                                println!("{label}> [tool result error={}]", r.is_error)
                            }
                            Part::Structured(v) => println!("{label}> [structured] {}", v.value),
                            Part::Media(_) | Part::File(_) | Part::Custom(_) => {
                                println!("{label}> [non-text part]")
                            }
                        }
                    }
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => return Ok(req),
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                return Err(
                    format!("unexpected approval interrupt: {}", pending.request.summary).into(),
                );
            }
        }
    }
}

fn parse_env_u32(key: &str) -> Option<u32> {
    env::var(key).ok().and_then(|v| v.parse().ok())
}

fn short(text: &str) -> String {
    let head: String = text.chars().take(80).collect();
    if text.chars().count() > 80 {
        format!("{head}...")
    } else {
        head
    }
}
