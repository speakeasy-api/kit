//! Interactive REPL against Anthropic's Messages API demonstrating the
//! `agentkit-provider-anthropic` feature surface.
//!
//! Streams tokens live (by default), supports server-side tools (web search,
//! web fetch, code execution), extended thinking, custom temperature, and a
//! system prompt. Ctrl-C cancels the in-flight turn; EOF or `/quit` (or
//! `/exit`) exits the loop.
//!
//! # Usage
//!
//! ```text
//! anthropic-chat [OPTIONS]
//!
//!   --no-streaming           Use the buffered (non-SSE) response path.
//!   --web-search [MAX_USES]  Enable the web_search server tool.
//!   --web-fetch  [MAX_USES]  Enable the web_fetch server tool.
//!   --code-exec              Enable the code_execution server tool.
//!   --thinking BUDGET        Enable extended thinking with the given budget.
//!   --system TEXT            Seed the transcript with a system prompt.
//!   --temperature F          Sampling temperature (0.0..=1.0).
//!   --help                   Print this help.
//! ```
//!
//! Credentials and model are read from the environment via
//! `AnthropicConfig::from_env` (`ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`,
//! `ANTHROPIC_MODEL`, `ANTHROPIC_MAX_TOKENS`, …).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_core::{CancellationController, Delta, Item, ItemKind, Part, Usage};
use agentkit_loop::{
    Agent, AgentEvent, InputRequest, LoopInterrupt, LoopObserver, LoopStep, ObservedEvent,
    PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_anthropic::{
    AnthropicAdapter, AnthropicConfig, CodeExecutionTool, ThinkingConfig, WebFetchTool,
    WebSearchTool, boxed,
};

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
    print_banner(&cli, &config);

    let adapter = AnthropicAdapter::new(config)?;
    let cancellation = CancellationController::new();
    let turn_active = Arc::new(AtomicBool::new(false));

    // `tokio::signal::ctrl_c` registers a process-wide SIGINT handler. Once
    // installed, Ctrl-C is intercepted even outside a turn, so a per-turn
    // listener (spawned then aborted) leaves the prompt with no subscriber
    // and the signal is silently dropped. Install a single long-lived
    // listener that routes: in-turn → cancel, idle → exit cleanly.
    spawn_signal_router(cancellation.clone(), Arc::clone(&turn_active));

    // Shared slot the observer fills with the final `Usage` for each turn.
    // `AgentEvent::UsageUpdated` fires synchronously from inside
    // `driver.next().await`, so a plain `Arc<Mutex<..>>` is sufficient.
    let usage_slot: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));

    println!(
        "Type a prompt and press enter. Use /exit or /quit to leave, Ctrl-C to cancel the current turn."
    );

    // Read the first user message before paying for a session.
    let Some(first_prompt) = read_prompt()? else {
        return Ok(());
    };

    // Seed the system prompt (if any) as passive prior transcript and
    // preload the first user message so the opening turn dispatches the
    // model directly without an AwaitingInput hop.
    let mut transcript = Vec::new();
    if let Some(system) = &cli.system {
        transcript.push(Item::text(ItemKind::System, system.clone()));
    }

    let agent = Agent::builder()
        .model(adapter)
        .cancellation(cancellation.handle())
        .observer(StreamPrinter::new(Arc::clone(&usage_slot)))
        .transcript(transcript)
        .input(vec![Item::text(ItemKind::User, first_prompt)])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("anthropic-chat").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    loop {
        usage_slot.lock().expect("usage slot poisoned").take();
        turn_active.store(true, Ordering::SeqCst);
        let result = run_turn(&mut driver, &usage_slot).await;
        turn_active.store(false, Ordering::SeqCst);
        let pending_input = result?;

        let Some(prompt) = read_prompt()? else {
            break;
        };
        pending_input.submit(&mut driver, vec![Item::text(ItemKind::User, prompt)])?;
    }

    Ok(())
}

/// Reads one non-empty prompt from stdin. Returns `Ok(None)` on EOF or on
/// `/exit` / `/quit`; loops on empty lines so the user can hit enter without
/// ending the session.
fn read_prompt() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut line = String::new();
    loop {
        print!("you> ");
        io::stdout().flush()?;
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            println!();
            return Ok(None);
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt == "/exit" || prompt == "/quit" {
            return Ok(None);
        }
        return Ok(Some(prompt.to_string()));
    }
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
) -> Result<InputRequest, Box<dyn std::error::Error>>
where
    S: agentkit_loop::ModelSession,
{
    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                println!();
                match result.finish_reason {
                    agentkit_core::FinishReason::Cancelled => eprintln!("[turn cancelled]"),
                    agentkit_core::FinishReason::Error => eprintln!("[turn ended with error]"),
                    _ => {}
                }

                if let Some(usage) = usage_slot
                    .lock()
                    .expect("usage slot poisoned")
                    .take()
                    .or(result.usage)
                {
                    print_usage_footer(&usage);
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => return Ok(req),
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                eprintln!("\n[approval required] {}", pending.request.summary);
                return Err("approval interrupt unhandled in anthropic-chat".into());
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
    let cache_write = tokens
        .cache_write_input_tokens
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into());
    println!(
        "[usage: input={} output={} cache_read={} cache_write={}]",
        tokens.input_tokens, tokens.output_tokens, cache_read, cache_write
    );
}

// --- CLI -------------------------------------------------------------------

struct CliArgs {
    streaming: bool,
    web_search: Option<Option<u32>>,
    web_fetch: Option<Option<u32>>,
    code_exec: bool,
    thinking_budget: Option<u32>,
    system: Option<String>,
    temperature: Option<f32>,
}

enum CliOutcome {
    Run(CliArgs),
    Help,
}

impl CliArgs {
    fn parse<I: IntoIterator<Item = String>>(
        args: I,
    ) -> Result<CliOutcome, Box<dyn std::error::Error>> {
        let mut cli = CliArgs {
            streaming: true,
            web_search: None,
            web_fetch: None,
            code_exec: false,
            thinking_budget: None,
            system: None,
            temperature: None,
        };

        let mut iter = args.into_iter().peekable();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--help" | "-h" => return Ok(CliOutcome::Help),
                "--streaming" => cli.streaming = true,
                "--no-streaming" => cli.streaming = false,
                "--web-search" => cli.web_search = Some(take_optional_u32(&mut iter)),
                "--web-fetch" => cli.web_fetch = Some(take_optional_u32(&mut iter)),
                "--code-exec" => cli.code_exec = true,
                "--thinking" => {
                    let budget = iter
                        .next()
                        .ok_or("--thinking requires a token budget")?
                        .parse::<u32>()?;
                    cli.thinking_budget = Some(budget);
                }
                "--system" => {
                    cli.system = Some(iter.next().ok_or("--system requires a prompt")?);
                }
                "--temperature" => {
                    let t = iter
                        .next()
                        .ok_or("--temperature requires a value")?
                        .parse::<f32>()?;
                    cli.temperature = Some(t);
                }
                other => return Err(format!("unknown argument: {other}").into()),
            }
        }

        Ok(CliOutcome::Run(cli))
    }
}

fn take_optional_u32(iter: &mut std::iter::Peekable<impl Iterator<Item = String>>) -> Option<u32> {
    match iter.peek() {
        Some(next) if !next.starts_with("--") => iter.next().and_then(|s| s.parse::<u32>().ok()),
        _ => None,
    }
}

fn print_help() {
    println!(
        "anthropic-chat — demonstrate agentkit-provider-anthropic\n\n\
         USAGE:\n    anthropic-chat [OPTIONS]\n\n\
         OPTIONS:\n    \
             --streaming              Use the SSE streaming path (default)\n    \
             --no-streaming           Use the buffered response path\n    \
             --web-search [MAX]       Enable the web_search server tool\n    \
             --web-fetch  [MAX]       Enable the web_fetch server tool\n    \
             --code-exec              Enable the code_execution server tool\n    \
             --thinking BUDGET        Enable extended thinking with a token budget\n    \
             --system TEXT            Seed the transcript with a system prompt\n    \
             --temperature F          Sampling temperature (0.0..=1.0)\n    \
             --help, -h               Print this help\n"
    );
}

fn build_config(cli: &CliArgs) -> Result<AnthropicConfig, Box<dyn std::error::Error>> {
    let mut config = AnthropicConfig::from_env()?.with_streaming(cli.streaming);

    if let Some(t) = cli.temperature {
        config = config.with_temperature(t);
    }

    if let Some(budget) = cli.thinking_budget {
        config = config.with_thinking(ThinkingConfig::Enabled {
            budget_tokens: budget,
        });
    }

    if let Some(max) = cli.web_search {
        let mut tool = WebSearchTool::new();
        if let Some(n) = max {
            tool = tool.with_max_uses(n);
        }
        config = config.with_server_tool(boxed(tool));
    }

    if let Some(max) = cli.web_fetch {
        let mut tool = WebFetchTool::new();
        if let Some(n) = max {
            tool = tool.with_max_uses(n);
        }
        config = config.with_server_tool(boxed(tool));
    }

    if cli.code_exec {
        config = config.with_server_tool(boxed(CodeExecutionTool::new()));
    }

    Ok(config)
}

fn print_banner(cli: &CliArgs, config: &AnthropicConfig) {
    println!("anthropic-chat");
    println!(
        "  model={} max_tokens={} streaming={} temperature={}",
        config.model,
        config.max_tokens,
        cli.streaming,
        cli.temperature
            .map(|t| format!("{t}"))
            .unwrap_or_else(|| "default".into()),
    );

    let mut features = Vec::new();
    if let Some(budget) = cli.thinking_budget {
        features.push(format!("thinking(budget={budget})"));
    }
    if let Some(max) = cli.web_search {
        features.push(format!(
            "web_search{}",
            max.map(|n| format!("(max={n})")).unwrap_or_default()
        ));
    }
    if let Some(max) = cli.web_fetch {
        features.push(format!(
            "web_fetch{}",
            max.map(|n| format!("(max={n})")).unwrap_or_default()
        ));
    }
    if cli.code_exec {
        features.push("code_execution".into());
    }
    if cli.system.is_some() {
        features.push("system_prompt".into());
    }
    if !features.is_empty() {
        println!("  features: {}", features.join(", "));
    }
}

// --- Streaming observer ----------------------------------------------------

/// Observer that streams text deltas to stdout live and prints short markers
/// when server-tool blocks commit.
///
/// The driver calls `handle_event` synchronously from inside `driver.next`,
/// so printing here happens as tokens arrive from the Anthropic SSE stream.
struct StreamPrinter {
    usage_slot: Arc<Mutex<Option<Usage>>>,
    state: Mutex<StreamPrinterState>,
}

struct StreamPrinterState {
    in_assistant_turn: bool,
    stdout: io::Stdout,
}

impl StreamPrinter {
    fn new(usage_slot: Arc<Mutex<Option<Usage>>>) -> Self {
        Self {
            usage_slot,
            state: Mutex::new(StreamPrinterState {
                in_assistant_turn: false,
                stdout: io::stdout(),
            }),
        }
    }
}

impl StreamPrinterState {
    fn ensure_prefix(&mut self) {
        if !self.in_assistant_turn {
            let _ = self.stdout.write_all(b"assistant> ");
            let _ = self.stdout.flush();
            self.in_assistant_turn = true;
        }
    }

    fn print_marker(&mut self, text: &str) {
        if self.in_assistant_turn {
            let _ = self.stdout.write_all(b"\n");
        }
        let _ = self.stdout.write_all(text.as_bytes());
        let _ = self.stdout.write_all(b"\n");
        let _ = self.stdout.flush();
        self.in_assistant_turn = false;
    }
}

impl LoopObserver for StreamPrinter {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        let mut state = self.state.lock().unwrap();
        match event {
            AgentEvent::TurnStarted { .. } => {
                state.in_assistant_turn = false;
            }
            AgentEvent::ContentDelta(Delta::AppendText { chunk, .. }) => {
                state.ensure_prefix();
                let _ = state.stdout.write_all(chunk.as_bytes());
                let _ = state.stdout.flush();
            }
            AgentEvent::ContentDelta(Delta::CommitPart { part }) => match part {
                Part::Text(t) if !state.in_assistant_turn => {
                    state.ensure_prefix();
                    let _ = state.stdout.write_all(t.text.as_bytes());
                    let _ = state.stdout.flush();
                }
                Part::Reasoning(r) if r.redacted => {
                    state.print_marker("[thinking: <redacted>]");
                }
                Part::Reasoning(r) if !state.in_assistant_turn => {
                    if let Some(summary) = &r.summary {
                        state.print_marker(&format!("[thinking]\n{summary}"));
                    }
                }
                Part::Custom(custom) if custom.kind.starts_with("anthropic.") => {
                    let suffix = custom.kind.trim_start_matches("anthropic.");
                    let name = custom
                        .value
                        .as_ref()
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str());
                    let marker = match name {
                        Some(name) => format!("[anthropic:{suffix} name={name}]"),
                        None => format!("[anthropic:{suffix}]"),
                    };
                    state.print_marker(&marker);
                }
                _ => {}
            },
            AgentEvent::ToolCallRequested(call) => {
                state.print_marker(&format!("[tool_call: {}]", call.name));
            }
            AgentEvent::UsageUpdated(usage) => {
                drop(state);
                if let Ok(mut slot) = self.usage_slot.lock() {
                    *slot = Some(usage);
                }
            }
            AgentEvent::TurnFinished(_) => {
                state.in_assistant_turn = false;
            }
            _ => {}
        }
    }
}
