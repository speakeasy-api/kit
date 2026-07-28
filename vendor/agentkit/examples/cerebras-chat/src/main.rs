//! Interactive REPL against Cerebras `/v1/chat/completions`, demonstrating the
//! full `agentkit-provider-cerebras` feature surface.
//!
//! Every `CerebrasConfig` knob has a CLI flag. Runtime state (rate-limit
//! snapshot, last-turn usage + `cerebras.*` metadata, model list, computed
//! request headers) is exposed via slash commands so a reviewer can verify
//! every feature without reading the crate source.
//!
//! # Env
//!
//! `CEREBRAS_API_KEY` + `CEREBRAS_MODEL` are required (read via
//! `CerebrasConfig::from_env`). CLI flags override env values.
//!
//! # Usage
//!
//! ```text
//! cerebras-chat [OPTIONS]
//! ```
//!
//! Run with `--help` to see every flag. Ctrl-C cancels the in-flight turn.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_core::{
    CancellationController, Delta, FinishReason, Item, ItemKind, Part, ToolCallId, ToolOutput,
    ToolResultPart, Usage,
};
use agentkit_loop::{
    Agent, AgentEvent, InputRequest, LoopInterrupt, LoopObserver, LoopStep, ObservedEvent,
    SessionConfig,
};
use agentkit_provider_cerebras::{
    CerebrasAdapter, CerebrasConfig, CompressionConfig, OutputFormat, Prediction, QueueThreshold,
    RateLimitSnapshot, ReasoningConfig, ReasoningEffort, ReasoningFormat, RequestEncoding,
    ServiceTier, ToolChoice,
};
use agentkit_tools_core::{
    Tool, ToolContext, ToolError, ToolName, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let cli = match CliArgs::parse(std::env::args().skip(1))? {
        CliOutcome::Run(cli) => cli,
        CliOutcome::Help => {
            print_help();
            return Ok(());
        }
    };

    let config = build_config(&cli)?;
    config.validate()?;

    let registry = build_registry(&cli);
    print_banner(&cli, &config);

    let adapter = CerebrasAdapter::new(config.clone())?;
    let cancellation = CancellationController::new();
    let turn_active = Arc::new(AtomicBool::new(false));
    spawn_signal_router(cancellation.clone(), Arc::clone(&turn_active));

    let usage_slot: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
    let last_usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));

    let streaming = config.streaming;

    println!(
        "Type a prompt and press enter. Slash: /show /usage /ratelimit /headers /models /reset /new /quit. Ctrl-C cancels in-flight turn."
    );

    // Outer loop: one iteration per session. /reset breaks back to here.
    'session: loop {
        // Read prompts until we have a real user message to seed the
        // session with. Slash commands handled inline; /reset on an
        // empty session is a no-op (already a fresh session).
        let first = loop {
            match read_input(&config, &adapter, &last_usage).await? {
                ReadResult::Quit => return Ok(()),
                ReadResult::Reset => continue,
                ReadResult::Prompt(p) => break p,
            }
        };

        let mut transcript = Vec::new();
        if let Some(system) = cli.system.as_deref() {
            transcript.push(Item::text(ItemKind::System, system.to_string()));
        }

        let mut driver = start_session(
            &adapter,
            &registry,
            &cancellation,
            streaming,
            &usage_slot,
            &last_usage,
            transcript,
            vec![Item::text(ItemKind::User, first)],
        )
        .await?;

        // Inner loop: run turns until /reset or /quit.
        loop {
            usage_slot.lock().expect("usage slot poisoned").take();
            turn_active.store(true, Ordering::SeqCst);
            let result = run_turn(&mut driver, &usage_slot, &last_usage).await;
            turn_active.store(false, Ordering::SeqCst);
            let pending_input = result?;

            let prompt = match read_input(&config, &adapter, &last_usage).await? {
                ReadResult::Quit => return Ok(()),
                ReadResult::Reset => {
                    println!("[session reset]");
                    continue 'session;
                }
                ReadResult::Prompt(p) => p,
            };
            pending_input.submit(&mut driver, vec![Item::text(ItemKind::User, prompt)])?;
        }
    }
}

enum ReadResult {
    Prompt(String),
    Reset,
    Quit,
}

/// Reads from stdin, handling slash commands inline. Returns when the user
/// has typed a real prompt, requested a reset, or asked to quit.
async fn read_input(
    config: &CerebrasConfig,
    adapter: &CerebrasAdapter,
    last_usage: &Arc<Mutex<Option<Usage>>>,
) -> Result<ReadResult, Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        print!("you> ");
        io::stdout().flush()?;
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            println!();
            return Ok(ReadResult::Quit);
        }
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        if raw == "/exit" || raw == "/quit" {
            return Ok(ReadResult::Quit);
        }
        if let Some(rest) = raw.strip_prefix('/') {
            match handle_slash(rest, config, adapter, last_usage).await {
                SlashOutcome::Handled => continue,
                SlashOutcome::Unknown => {
                    eprintln!("unknown command: /{rest}");
                    continue;
                }
                SlashOutcome::Reset => return Ok(ReadResult::Reset),
            }
        }
        return Ok(ReadResult::Prompt(raw.to_string()));
    }
}

async fn start_session(
    adapter: &CerebrasAdapter,
    registry: &ToolRegistry,
    cancellation: &CancellationController,
    streaming: bool,
    usage_slot: &Arc<Mutex<Option<Usage>>>,
    last_usage: &Arc<Mutex<Option<Usage>>>,
    transcript: Vec<Item>,
    input: Vec<Item>,
) -> Result<
    agentkit_loop::LoopDriver<agentkit_provider_cerebras::CerebrasSession>,
    Box<dyn std::error::Error>,
> {
    let agent = Agent::builder()
        .model(adapter.clone())
        .cancellation(cancellation.handle())
        .add_tool_source(registry.clone())
        .observer(StreamPrinter::new(
            streaming,
            Arc::clone(usage_slot),
            Arc::clone(last_usage),
        ))
        .transcript(transcript)
        .input(input)
        .build()?;
    let driver = agent.start(SessionConfig::new("cerebras-chat")).await?;
    Ok(driver)
}

fn spawn_signal_router(cancellation: CancellationController, turn_active: Arc<AtomicBool>) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            if turn_active.load(Ordering::SeqCst) {
                cancellation.interrupt();
            } else {
                eprintln!("\n[exit]");
                std::process::exit(0);
            }
        }
    });
}

async fn run_turn<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
    usage_slot: &Arc<Mutex<Option<Usage>>>,
    last_usage: &Arc<Mutex<Option<Usage>>>,
) -> Result<InputRequest, Box<dyn std::error::Error>>
where
    S: agentkit_loop::ModelSession,
{
    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                println!();
                match result.finish_reason {
                    FinishReason::Cancelled => eprintln!("[turn cancelled]"),
                    FinishReason::Error => eprintln!("[turn ended with error]"),
                    _ => {}
                }
                if let Some(usage) = usage_slot
                    .lock()
                    .expect("usage slot poisoned")
                    .take()
                    .or(result.usage)
                {
                    print_usage_footer(&usage);
                    *last_usage.lock().expect("last usage poisoned") = Some(usage);
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => return Ok(req),
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                eprintln!("\n[approval required] {}", pending.request.summary);
                return Err("approval interrupt unhandled in cerebras-chat".into());
            }
        }
    }
}

fn print_usage_footer(usage: &Usage) {
    let Some(tokens) = usage.tokens.as_ref() else {
        return;
    };
    let cache_read = tokens
        .cached_input_tokens
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into());
    let reasoning = tokens
        .reasoning_tokens
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into());
    println!(
        "[usage: input={} output={} cache_read={} reasoning={}]",
        tokens.input_tokens, tokens.output_tokens, cache_read, reasoning
    );
}

// --- Slash commands --------------------------------------------------------

enum SlashOutcome {
    Handled,
    Unknown,
    Reset,
}

async fn handle_slash(
    command: &str,
    config: &CerebrasConfig,
    adapter: &CerebrasAdapter,
    last_usage: &Arc<Mutex<Option<Usage>>>,
) -> SlashOutcome {
    let mut tokens = command.split_whitespace();
    let Some(head) = tokens.next() else {
        return SlashOutcome::Unknown;
    };
    match head {
        "show" => {
            print_show(config);
            SlashOutcome::Handled
        }
        "usage" => {
            match last_usage.lock().expect("last usage poisoned").as_ref() {
                Some(u) => print_full_usage(u),
                None => println!("[no usage recorded yet]"),
            }
            SlashOutcome::Handled
        }
        "ratelimit" => {
            match adapter.last_rate_limit() {
                Some(snap) => print_ratelimit(&snap),
                None => println!("[no rate-limit snapshot yet]"),
            }
            SlashOutcome::Handled
        }
        "headers" => {
            print_headers(config);
            SlashOutcome::Handled
        }
        "models" => match adapter.models().list().await {
            Ok(models) => {
                println!("[models]");
                for m in models {
                    println!("  {} (owner={})", m.id, m.owned_by);
                }
                SlashOutcome::Handled
            }
            Err(e) => {
                eprintln!("[models error] {e}");
                SlashOutcome::Handled
            }
        },
        "reset" | "new" => SlashOutcome::Reset,
        "cancel" => {
            println!("[cancel only applies mid-turn — press Ctrl-C while a response is streaming]");
            SlashOutcome::Handled
        }
        _ => SlashOutcome::Unknown,
    }
}

fn print_show(config: &CerebrasConfig) {
    let mut json = serde_json::Map::new();
    json.insert("model".into(), json!(config.model));
    json.insert("base_url".into(), json!(config.base_url));
    json.insert("streaming".into(), json!(config.streaming));
    json.insert("api_key".into(), json!("***redacted***"));
    if let Some(v) = config.version_patch {
        json.insert("version_patch".into(), json!(v));
    }
    if let Some(v) = config.max_completion_tokens {
        json.insert("max_completion_tokens".into(), json!(v));
    }
    if let Some(v) = config.min_tokens {
        json.insert("min_tokens".into(), json!(v));
    }
    if let Some(v) = config.temperature {
        json.insert("temperature".into(), json!(v));
    }
    if let Some(v) = config.top_p {
        json.insert("top_p".into(), json!(v));
    }
    if let Some(v) = config.frequency_penalty {
        json.insert("frequency_penalty".into(), json!(v));
    }
    if let Some(v) = config.presence_penalty {
        json.insert("presence_penalty".into(), json!(v));
    }
    if let Some(stops) = &config.stop {
        json.insert("stop".into(), json!(stops));
    }
    if let Some(v) = config.seed {
        json.insert("seed".into(), json!(v));
    }
    if let Some(bias) = &config.logit_bias {
        json.insert("logit_bias".into(), json!(bias));
    }
    if let Some(v) = config.logprobs {
        json.insert("logprobs".into(), json!(v));
    }
    if let Some(v) = config.top_logprobs {
        json.insert("top_logprobs".into(), json!(v));
    }
    if let Some(u) = &config.user {
        json.insert("user".into(), json!(u));
    }
    if let Some(choice) = &config.tool_choice {
        json.insert("tool_choice".into(), json!(format!("{choice:?}")));
    }
    if let Some(v) = config.parallel_tool_calls {
        json.insert("parallel_tool_calls".into(), json!(v));
    }
    json.insert("tool_strict".into(), json!(config.tool_strict));
    if let Some(fmt) = &config.output_format {
        json.insert("output_format".into(), json!(format!("{fmt:?}")));
    }
    if let Some(r) = &config.reasoning {
        json.insert(
            "reasoning".into(),
            json!({
                "effort": r.effort.map(|e| format!("{e:?}")),
                "format": r.format.map(|f| format!("{f:?}")),
                "clear_thinking": r.clear_thinking,
                "disable_reasoning": r.disable_reasoning,
            }),
        );
    }
    if let Some(p) = &config.prediction {
        json.insert("prediction".into(), json!(format!("{p:?}")));
    }
    if let Some(t) = config.service_tier {
        json.insert("service_tier".into(), json!(format!("{t:?}")));
    }
    if let Some(ms) = config.queue_threshold_ms {
        json.insert("queue_threshold_ms".into(), json!(ms));
    }
    if let Some(c) = &config.compression {
        json.insert(
            "compression".into(),
            json!({
                "encoding": format!("{:?}", c.encoding),
                "min_bytes": c.min_bytes,
            }),
        );
    }
    if !config.extra_headers.is_empty() {
        let redacted: Vec<_> = config
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), redact_header_value(k, v)))
            .collect();
        json.insert("extra_headers".into(), json!(redacted));
    }
    if let Some(extra) = &config.extra_body {
        json.insert("extra_body".into(), extra.clone());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&Value::Object(json)).unwrap()
    );
}

fn print_full_usage(usage: &Usage) {
    println!("[last turn usage]");
    if let Some(t) = &usage.tokens {
        println!(
            "  input={} output={} cached={:?} reasoning={:?} cache_write={:?}",
            t.input_tokens,
            t.output_tokens,
            t.cached_input_tokens,
            t.reasoning_tokens,
            t.cache_write_input_tokens,
        );
    }
    for (key, value) in &usage.metadata {
        if key.starts_with("cerebras.") {
            println!("  {key} = {value}");
        }
    }
}

fn print_ratelimit(snap: &RateLimitSnapshot) {
    println!("[rate-limit snapshot]");
    println!(
        "  requests/day: {}/{} (reset: {})",
        snap.requests_day_remaining
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        snap.requests_day_limit
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        snap.requests_day_reset.as_deref().unwrap_or("-"),
    );
    println!(
        "  tokens/min:   {}/{} (reset: {})",
        snap.tokens_minute_remaining
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        snap.tokens_minute_limit
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into()),
        snap.tokens_minute_reset.as_deref().unwrap_or("-"),
    );
}

fn print_headers(config: &CerebrasConfig) {
    println!("[computed request headers — what the adapter would send for the next turn]");
    println!("  Authorization: Bearer ***redacted***");
    match &config.compression {
        Some(c) => {
            let (ct, enc) = match c.encoding {
                RequestEncoding::Json => ("application/json", None),
                RequestEncoding::Msgpack => ("application/vnd.msgpack", None),
                RequestEncoding::JsonGzip => ("application/json", Some("gzip")),
                RequestEncoding::MsgpackGzip => ("application/vnd.msgpack", Some("gzip")),
            };
            println!("  Content-Type: {ct}");
            if let Some(e) = enc {
                println!("  Content-Encoding: {e}");
            }
        }
        None => println!("  Content-Type: application/json"),
    }
    if let Some(v) = config.version_patch {
        println!("  X-Cerebras-Version-Patch: {v}");
    }
    if let Some(ms) = config.queue_threshold_ms {
        println!("  queue_threshold: {ms}");
    }
    println!(
        "  User-Agent: agentkit-provider-cerebras/{}",
        env!("CARGO_PKG_VERSION")
    );
    if config.streaming {
        println!("  Accept: text/event-stream");
    }
    for (k, v) in &config.extra_headers {
        println!("  {k}: {}", redact_header_value(k, v));
    }
}

fn redact_header_value(key: &str, value: &str) -> String {
    if key.eq_ignore_ascii_case("authorization") || key.to_ascii_lowercase().contains("key") {
        "***redacted***".into()
    } else {
        value.to_string()
    }
}

// --- CLI -------------------------------------------------------------------

struct CliArgs {
    // auth & transport
    model: Option<String>,
    base_url: Option<String>,
    version_patch: Option<u32>,
    extra_headers: Vec<(String, String)>,
    extra_body: Option<Value>,
    // model / tokens
    max_completion_tokens: Option<u32>,
    min_tokens: Option<i32>,
    // sampling
    temperature: Option<f32>,
    top_p: Option<f32>,
    frequency_penalty: Option<f32>,
    presence_penalty: Option<f32>,
    stop: Vec<String>,
    seed: Option<i64>,
    logit_bias: BTreeMap<String, i32>,
    logprobs: bool,
    top_logprobs: Option<u32>,
    user: Option<String>,
    // tools
    tool_choice: Option<ToolChoice>,
    no_parallel_tool_calls: bool,
    tool_strict: bool,
    tools: Vec<(String, Value)>,
    // output
    response_format: Option<OutputFormat>,
    // reasoning
    reasoning_effort: Option<ReasoningEffort>,
    reasoning_format: Option<ReasoningFormat>,
    clear_thinking: Option<bool>,
    disable_reasoning: bool,
    // streaming
    streaming: bool,
    // preview-gated
    prediction: Option<String>,
    service_tier: Option<ServiceTier>,
    queue_threshold_ms: Option<u32>,
    compression: Option<RequestEncoding>,
    compression_min_bytes: Option<usize>,
    // misc
    system: Option<String>,
}

enum CliOutcome {
    Run(Box<CliArgs>),
    Help,
}

impl CliArgs {
    fn parse<I: IntoIterator<Item = String>>(
        args: I,
    ) -> Result<CliOutcome, Box<dyn std::error::Error>> {
        let mut cli = CliArgs {
            model: None,
            base_url: None,
            version_patch: None,
            extra_headers: Vec::new(),
            extra_body: None,
            max_completion_tokens: None,
            min_tokens: None,
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: Vec::new(),
            seed: None,
            logit_bias: BTreeMap::new(),
            logprobs: false,
            top_logprobs: None,
            user: None,
            tool_choice: None,
            no_parallel_tool_calls: false,
            tool_strict: false,
            tools: Vec::new(),
            response_format: None,
            reasoning_effort: None,
            reasoning_format: None,
            clear_thinking: None,
            disable_reasoning: false,
            streaming: true,
            prediction: None,
            service_tier: None,
            queue_threshold_ms: None,
            compression: None,
            compression_min_bytes: None,
            system: None,
        };

        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--help" | "-h" => return Ok(CliOutcome::Help),
                "--model" => cli.model = Some(need(&mut iter, "--model")?),
                "--base-url" => cli.base_url = Some(need(&mut iter, "--base-url")?),
                "--version-patch" => {
                    cli.version_patch = Some(need(&mut iter, "--version-patch")?.parse()?)
                }
                "--extra-header" => {
                    let kv = need(&mut iter, "--extra-header")?;
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or("--extra-header expects key=value")?;
                    cli.extra_headers.push((k.into(), v.into()));
                }
                "--extra-body" => {
                    let path = need(&mut iter, "--extra-body")?;
                    let text = std::fs::read_to_string(&path)?;
                    cli.extra_body = Some(serde_json::from_str(&text)?);
                }
                "--max-completion-tokens" => {
                    cli.max_completion_tokens =
                        Some(need(&mut iter, "--max-completion-tokens")?.parse()?)
                }
                "--min-tokens" => cli.min_tokens = Some(need(&mut iter, "--min-tokens")?.parse()?),
                "--temperature" => {
                    cli.temperature = Some(need(&mut iter, "--temperature")?.parse()?)
                }
                "--top-p" => cli.top_p = Some(need(&mut iter, "--top-p")?.parse()?),
                "--frequency-penalty" => {
                    cli.frequency_penalty = Some(need(&mut iter, "--frequency-penalty")?.parse()?)
                }
                "--presence-penalty" => {
                    cli.presence_penalty = Some(need(&mut iter, "--presence-penalty")?.parse()?)
                }
                "--stop" => {
                    if cli.stop.len() >= 4 {
                        return Err("--stop supports at most 4 entries".into());
                    }
                    cli.stop.push(need(&mut iter, "--stop")?);
                }
                "--seed" => cli.seed = Some(need(&mut iter, "--seed")?.parse()?),
                "--logit-bias" => {
                    let kv = need(&mut iter, "--logit-bias")?;
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or("--logit-bias expects token_id=bias")?;
                    cli.logit_bias.insert(k.into(), v.parse()?);
                }
                "--logprobs" => cli.logprobs = true,
                "--top-logprobs" => {
                    cli.top_logprobs = Some(need(&mut iter, "--top-logprobs")?.parse()?)
                }
                "--user" => cli.user = Some(need(&mut iter, "--user")?),
                "--tool-choice" => {
                    let raw = need(&mut iter, "--tool-choice")?;
                    cli.tool_choice = Some(parse_tool_choice(&raw)?);
                }
                "--no-parallel-tool-calls" => cli.no_parallel_tool_calls = true,
                "--tool-strict" => cli.tool_strict = true,
                "--tool" => {
                    let raw = need(&mut iter, "--tool")?;
                    let (name, path) =
                        raw.split_once('=').ok_or("--tool expects name=path.json")?;
                    let text = std::fs::read_to_string(path)?;
                    let schema: Value = serde_json::from_str(&text)?;
                    cli.tools.push((name.into(), schema));
                }
                "--response-format" => {
                    let raw = need(&mut iter, "--response-format")?;
                    cli.response_format = Some(parse_response_format(&raw)?);
                }
                "--reasoning-effort" => {
                    let raw = need(&mut iter, "--reasoning-effort")?;
                    cli.reasoning_effort = Some(parse_reasoning_effort(&raw)?);
                }
                "--reasoning-format" => {
                    let raw = need(&mut iter, "--reasoning-format")?;
                    cli.reasoning_format = Some(parse_reasoning_format(&raw)?);
                }
                "--clear-thinking" => {
                    let raw = need(&mut iter, "--clear-thinking")?;
                    cli.clear_thinking = Some(raw.parse()?);
                }
                "--disable-reasoning" => cli.disable_reasoning = true,
                "--no-streaming" => cli.streaming = false,
                "--streaming" => cli.streaming = true,
                "--prediction" => {
                    let path = need(&mut iter, "--prediction")?;
                    cli.prediction = Some(std::fs::read_to_string(&path)?);
                }
                "--service-tier" => {
                    let raw = need(&mut iter, "--service-tier")?;
                    cli.service_tier = Some(parse_service_tier(&raw)?);
                }
                "--queue-threshold-ms" => {
                    cli.queue_threshold_ms = Some(need(&mut iter, "--queue-threshold-ms")?.parse()?)
                }
                "--compression" => {
                    let raw = need(&mut iter, "--compression")?;
                    cli.compression = Some(parse_compression(&raw)?);
                }
                "--compression-min-bytes" => {
                    cli.compression_min_bytes =
                        Some(need(&mut iter, "--compression-min-bytes")?.parse()?)
                }
                "--system" => cli.system = Some(need(&mut iter, "--system")?),
                other => return Err(format!("unknown argument: {other}").into()),
            }
        }

        Ok(CliOutcome::Run(Box::new(cli)))
    }
}

fn need<I: Iterator<Item = String>>(
    iter: &mut I,
    flag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value").into())
}

fn parse_tool_choice(raw: &str) -> Result<ToolChoice, Box<dyn std::error::Error>> {
    match raw {
        "auto" => Ok(ToolChoice::Auto),
        "none" => Ok(ToolChoice::None),
        "required" => Ok(ToolChoice::Required),
        other if other.starts_with("tool:") => Ok(ToolChoice::Function {
            name: other.trim_start_matches("tool:").into(),
        }),
        _ => Err(format!("bad --tool-choice: {raw}").into()),
    }
}

fn parse_response_format(raw: &str) -> Result<OutputFormat, Box<dyn std::error::Error>> {
    match raw {
        "text" => Ok(OutputFormat::Text),
        "json_object" => Ok(OutputFormat::JsonObject),
        other if other.starts_with("json_schema:") => {
            let path = other.trim_start_matches("json_schema:");
            let text = std::fs::read_to_string(path)?;
            let schema: Value = serde_json::from_str(&text)?;
            Ok(OutputFormat::JsonSchema {
                schema,
                strict: true,
                name: None,
            })
        }
        _ => Err(format!("bad --response-format: {raw}").into()),
    }
}

fn parse_reasoning_effort(raw: &str) -> Result<ReasoningEffort, Box<dyn std::error::Error>> {
    Ok(match raw {
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "none" => ReasoningEffort::None,
        _ => return Err(format!("bad --reasoning-effort: {raw}").into()),
    })
}

fn parse_reasoning_format(raw: &str) -> Result<ReasoningFormat, Box<dyn std::error::Error>> {
    Ok(match raw {
        "parsed" => ReasoningFormat::Parsed,
        "raw" => ReasoningFormat::Raw,
        "hidden" => ReasoningFormat::Hidden,
        "none" => ReasoningFormat::None,
        _ => return Err(format!("bad --reasoning-format: {raw}").into()),
    })
}

fn parse_service_tier(raw: &str) -> Result<ServiceTier, Box<dyn std::error::Error>> {
    Ok(match raw {
        "priority" => ServiceTier::Priority,
        "default" => ServiceTier::Default,
        "auto" => ServiceTier::Auto,
        "flex" => ServiceTier::Flex,
        _ => return Err(format!("bad --service-tier: {raw}").into()),
    })
}

fn parse_compression(raw: &str) -> Result<RequestEncoding, Box<dyn std::error::Error>> {
    Ok(match raw {
        "none" | "json" => RequestEncoding::Json,
        "msgpack" => RequestEncoding::Msgpack,
        "gzip" | "json+gzip" => RequestEncoding::JsonGzip,
        "msgpack+gzip" => RequestEncoding::MsgpackGzip,
        _ => return Err(format!("bad --compression: {raw}").into()),
    })
}

fn print_help() {
    println!(
        "cerebras-chat — exercise every agentkit-provider-cerebras knob\n\n\
         USAGE:\n    cerebras-chat [OPTIONS]\n\n\
         TRANSPORT:\n    \
             --model ID                   Override CEREBRAS_MODEL\n    \
             --base-url URL               Override base URL\n    \
             --version-patch N            X-Cerebras-Version-Patch\n    \
             --extra-header K=V           (repeatable)\n    \
             --extra-body PATH            Deep-merged into request body\n\n\
         TOKENS / SAMPLING:\n    \
             --max-completion-tokens N\n    \
             --min-tokens N               -1 allowed as sentinel\n    \
             --temperature F              0.0..=2.0\n    \
             --top-p F\n    \
             --frequency-penalty F        -2.0..=2.0\n    \
             --presence-penalty F         -2.0..=2.0\n    \
             --stop STR                   (repeatable, max 4)\n    \
             --seed N\n    \
             --logit-bias TOKEN_ID=BIAS   (repeatable)\n    \
             --logprobs\n    \
             --top-logprobs N             Requires --logprobs\n    \
             --user ID\n\n\
         TOOLS:\n    \
             --tool-choice VAL            auto | none | required | tool:<name>\n    \
             --no-parallel-tool-calls\n    \
             --tool-strict\n    \
             --tool NAME=schema.json      Register a local pass-through tool\n\n\
         OUTPUT:\n    \
             --response-format VAL        text | json_object | json_schema:<path>\n\n\
         REASONING:\n    \
             --reasoning-effort VAL       low | medium | high | none\n    \
             --reasoning-format VAL       parsed | raw | hidden | none\n    \
             --clear-thinking BOOL\n    \
             --disable-reasoning\n\n\
         STREAM:\n    \
             --no-streaming\n\n\
         PREVIEW:\n    \
             --prediction PATH            Literal text to use as prediction\n    \
             --service-tier VAL           priority | default | auto | flex\n    \
             --queue-threshold-ms N       50..=20000\n    \
             --compression VAL            none | msgpack | gzip | msgpack+gzip\n    \
             --compression-min-bytes N\n\n\
         MISC:\n    \
             --system TEXT                Seed the transcript with a system prompt\n    \
             --help, -h                   Print this help\n"
    );
}

fn build_config(cli: &CliArgs) -> Result<CerebrasConfig, Box<dyn std::error::Error>> {
    let mut config = match CerebrasConfig::from_env() {
        Ok(cfg) => cfg,
        Err(_) => {
            let model = cli
                .model
                .clone()
                .ok_or("CEREBRAS_MODEL not set and --model not provided")?;
            let api_key =
                std::env::var("CEREBRAS_API_KEY").map_err(|_| "CEREBRAS_API_KEY not set")?;
            CerebrasConfig::new(api_key, model)?
        }
    };
    if let Some(m) = &cli.model {
        config.model = m.clone();
    }
    if let Some(url) = &cli.base_url {
        config.base_url = url.clone();
    }
    if let Some(v) = cli.version_patch {
        config.version_patch = Some(v);
    }
    for (k, v) in &cli.extra_headers {
        config.extra_headers.push((k.clone(), v.clone()));
    }
    if let Some(extra) = &cli.extra_body {
        config.extra_body = Some(extra.clone());
    }
    if let Some(v) = cli.max_completion_tokens {
        config.max_completion_tokens = Some(v);
    }
    if let Some(v) = cli.min_tokens {
        config.min_tokens = Some(v);
    }
    if let Some(v) = cli.temperature {
        config.temperature = Some(v);
    }
    if let Some(v) = cli.top_p {
        config.top_p = Some(v);
    }
    if let Some(v) = cli.frequency_penalty {
        config.frequency_penalty = Some(v);
    }
    if let Some(v) = cli.presence_penalty {
        config.presence_penalty = Some(v);
    }
    if !cli.stop.is_empty() {
        config.stop = Some(cli.stop.clone());
    }
    if let Some(v) = cli.seed {
        config.seed = Some(v);
    }
    if !cli.logit_bias.is_empty() {
        config.logit_bias = Some(cli.logit_bias.clone());
    }
    if cli.logprobs {
        config.logprobs = Some(true);
    }
    if let Some(v) = cli.top_logprobs {
        config.top_logprobs = Some(v);
    }
    if let Some(u) = &cli.user {
        config.user = Some(u.clone());
    }
    if let Some(choice) = &cli.tool_choice {
        config.tool_choice = Some(choice.clone());
    }
    if cli.no_parallel_tool_calls {
        config.parallel_tool_calls = Some(false);
    }
    if cli.tool_strict {
        config.tool_strict = true;
    }
    if let Some(fmt) = &cli.response_format {
        config.output_format = Some(fmt.clone());
    }
    if cli.reasoning_effort.is_some()
        || cli.reasoning_format.is_some()
        || cli.clear_thinking.is_some()
        || cli.disable_reasoning
    {
        let mut r = ReasoningConfig::new();
        if let Some(e) = cli.reasoning_effort {
            r = r.with_effort(e);
        }
        if let Some(f) = cli.reasoning_format {
            r = r.with_format(f);
        }
        if let Some(b) = cli.clear_thinking {
            r = r.with_clear_thinking(b);
        }
        if cli.disable_reasoning {
            r = r.with_disable_reasoning(true);
        }
        config.reasoning = Some(r);
    }
    config.streaming = cli.streaming;
    if let Some(text) = &cli.prediction {
        config.prediction = Some(Prediction::Content(text.clone()));
    }
    if let Some(tier) = cli.service_tier {
        config.service_tier = Some(tier);
    }
    if let Some(ms) = cli.queue_threshold_ms {
        config.queue_threshold_ms = Some(ms);
        let _ = QueueThreshold(ms);
    }
    if let Some(enc) = cli.compression {
        let mut c = CompressionConfig::new(enc);
        if let Some(n) = cli.compression_min_bytes {
            c.min_bytes = n;
        }
        config.compression = Some(c);
    }
    Ok(config)
}

fn build_registry(cli: &CliArgs) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for (name, schema) in &cli.tools {
        registry.register(PassthroughTool::new(name, schema.clone()));
    }
    registry
}

fn print_banner(cli: &CliArgs, config: &CerebrasConfig) {
    println!("cerebras-chat");
    println!(
        "  model={} base_url={} streaming={}",
        config.model, config.base_url, config.streaming
    );
    let mut features = Vec::new();
    if let Some(v) = config.max_completion_tokens {
        features.push(format!("max_completion_tokens={v}"));
    }
    if let Some(v) = config.temperature {
        features.push(format!("temperature={v}"));
    }
    if let Some(v) = config.version_patch {
        features.push(format!("version_patch={v}"));
    }
    if config.tool_strict {
        features.push("tool_strict".into());
    }
    if let Some(c) = &config.tool_choice {
        features.push(format!("tool_choice={c:?}"));
    }
    if let Some(fmt) = &config.output_format {
        features.push(format!("output_format={fmt:?}"));
    }
    if let Some(r) = &config.reasoning {
        features.push(format!(
            "reasoning(effort={:?}, format={:?}, clear_thinking={:?})",
            r.effort, r.format, r.clear_thinking
        ));
    }
    if config.prediction.is_some() {
        features.push("prediction".into());
    }
    if let Some(t) = config.service_tier {
        features.push(format!("service_tier={t:?}"));
    }
    if let Some(ms) = config.queue_threshold_ms {
        features.push(format!("queue_threshold_ms={ms}"));
    }
    if let Some(c) = &config.compression {
        features.push(format!(
            "compression={:?}(min_bytes={})",
            c.encoding, c.min_bytes
        ));
    }
    if !cli.tools.is_empty() {
        let names: Vec<_> = cli.tools.iter().map(|(n, _)| n.clone()).collect();
        features.push(format!("tools=[{}]", names.join(",")));
    }
    if cli.system.is_some() {
        features.push("system_prompt".into());
    }
    if !features.is_empty() {
        println!("  features: {}", features.join(", "));
    }
}

// --- Streaming observer ----------------------------------------------------

/// Observer that renders streaming deltas to stdout: content inline,
/// reasoning in a leading block, tool-calls as bracketed markers.
/// Observer that renders deltas to stdout.
///
/// Contract:
/// - Streaming path: render `AppendText` chunks; drop `CommitPart` for
///   variants that also stream (`Text`, `Reasoning`) — they are redundant
///   with the buffer we already printed.
/// - Buffered path (`--no-streaming`): no `AppendText` deltas arrive, so
///   every part is rendered from its `CommitPart`.
/// - Non-streamable parts (tool calls, structured, custom) are always
///   rendered from `CommitPart`, regardless of mode.
struct StreamPrinter {
    usage_slot: Arc<Mutex<Option<Usage>>>,
    last_usage: Arc<Mutex<Option<Usage>>>,
    streaming: bool,
    state: Mutex<StreamPrinterState>,
}

struct StreamPrinterState {
    in_assistant_turn: bool,
    in_reasoning: bool,
    stdout: io::Stdout,
}

impl StreamPrinter {
    fn new(
        streaming: bool,
        usage_slot: Arc<Mutex<Option<Usage>>>,
        last_usage: Arc<Mutex<Option<Usage>>>,
    ) -> Self {
        Self {
            usage_slot,
            last_usage,
            streaming,
            state: Mutex::new(StreamPrinterState {
                in_assistant_turn: false,
                in_reasoning: false,
                stdout: io::stdout(),
            }),
        }
    }
}

impl StreamPrinterState {
    fn write(&mut self, bytes: &[u8]) {
        let _ = self.stdout.write_all(bytes);
        let _ = self.stdout.flush();
    }

    fn ensure_assistant_prefix(&mut self) {
        if self.in_reasoning {
            self.write(b"\n");
            self.in_reasoning = false;
        }
        if !self.in_assistant_turn {
            self.write(b"assistant> ");
            self.in_assistant_turn = true;
        }
    }

    fn ensure_reasoning_prefix(&mut self) {
        if !self.in_reasoning && !self.in_assistant_turn {
            self.write(b"[reasoning]\n");
            self.in_reasoning = true;
        }
    }
}

impl LoopObserver for StreamPrinter {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        let mut state = self.state.lock().unwrap();
        match event {
            AgentEvent::TurnStarted { .. } => {
                state.in_assistant_turn = false;
                state.in_reasoning = false;
            }
            AgentEvent::ContentDelta(Delta::AppendText { part_id, chunk }) => {
                if part_id.0.contains("reasoning") {
                    state.ensure_reasoning_prefix();
                } else {
                    state.ensure_assistant_prefix();
                }
                state.write(chunk.as_bytes());
            }
            AgentEvent::ContentDelta(Delta::CommitPart { part }) => match part {
                Part::Text(_) | Part::Reasoning(_) if self.streaming => {}
                Part::Text(t) => {
                    state.ensure_assistant_prefix();
                    state.write(t.text.as_bytes());
                }
                Part::Reasoning(r) => {
                    if let Some(s) = &r.summary {
                        state.write(b"[reasoning]\n");
                        state.write(s.as_bytes());
                        state.write(b"\n");
                    }
                }
                _ => {}
            },
            AgentEvent::ToolCallRequested(call) => {
                if state.in_assistant_turn || state.in_reasoning {
                    state.write(b"\n");
                }
                let msg = format!("[tool_call: {} {}]\n", call.name, call.input);
                state.write(msg.as_bytes());
                state.in_assistant_turn = false;
                state.in_reasoning = false;
            }
            AgentEvent::UsageUpdated(usage) => {
                drop(state);
                if let Ok(mut slot) = self.usage_slot.lock() {
                    *slot = Some(usage.clone());
                }
                if let Ok(mut slot) = self.last_usage.lock() {
                    *slot = Some(usage);
                }
            }
            AgentEvent::TurnFinished(_) => {
                state.in_assistant_turn = false;
                state.in_reasoning = false;
            }
            _ => {}
        }
    }
}

// --- Local pass-through tool ----------------------------------------------

/// Minimal `Tool` implementation: echoes the model's input back as the result.
/// Proves end-to-end tool-plumbing without pulling in any real behaviour.
struct PassthroughTool {
    spec: ToolSpec,
}

impl PassthroughTool {
    fn new(name: &str, schema: Value) -> Self {
        Self {
            spec: ToolSpec::new(
                ToolName::new(name),
                format!("Echoes its input back. Declared via --tool {name}"),
                schema,
            ),
        }
    }
}

#[async_trait]
impl Tool for PassthroughTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let call_id: ToolCallId = request.call_id;
        Ok(ToolResult::new(ToolResultPart::success(
            call_id,
            ToolOutput::Structured(request.input),
        )))
    }
}
