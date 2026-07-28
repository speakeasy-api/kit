//! Claude-Code-style REPL on top of the agentkit loop.
//!
//! # Architecture
//!
//! This example deliberately demonstrates the **actor / command-channel**
//! pattern as the production-quality way to build an interactive agent:
//!
//! ```text
//!   ┌──────────────┐   AgentCommand    ┌───────────────────┐
//!   │   UI task    │ ────────────────▶ │   Agent task      │
//!   │ (stdin/TTY)  │                   │ (owns LoopDriver) │
//!   │              │ ◀──────────────── │                   │
//!   └──────────────┘     UiEvent       └───────────────────┘
//! ```
//!
//! Responsibilities are split so each task owns one well-scoped concern:
//!
//! - **Agent task** — `run_agent`. Owns the single `&mut LoopDriver`. Runs
//!   a state machine (`Mode::Idle` / `Driving` / `AwaitingApproval`) driven
//!   by `AgentCommand`s from the UI and by [`LoopStep`] returns from the
//!   driver. Knows nothing about terminal rendering, prompts, or key codes.
//!
//! - **UI task** — `run_ui`. Owns stdin, stdout, and the local
//!   `UiMode::{MessageInput, ApprovalInput}` state. Renders [`AgentEvent`]s
//!   forwarded via a [`LoopObserver`] into a channel, classifies each stdin
//!   line by the current [`UiMode`], and emits typed `AgentCommand`s. Knows
//!   nothing about the driver, transcripts, or cancellation primitives.
//!
//! - **Observers** — two small [`LoopObserver`]s live in the agent task:
//!   [`MeterObserver`] records token usage into the shared [`TokenMeter`]
//!   (read by [`TokenBudgetTrigger`]); [`ChannelObserver`] forwards every
//!   [`AgentEvent`] into the UI task for rendering.
//!
//! # Why two tasks instead of one nested loop
//!
//! The `LoopDriver` requires `&mut self` for both `next()` and
//! `submit_input()`.  A single-task REPL has to solve this with
//! `tokio::select!` plus local buffering (see git history of this file).
//! The actor split keeps the same driver invariants but makes the two
//! concerns — "owner of the driver" and "owner of the terminal" —
//! physically separate, which yields:
//!
//! - A typed command API that is trivial to swap for an HTTP handler, a
//!   test harness, or a different front-end, without touching the agent
//!   code.
//! - A UI task whose only input classification decision is "which
//!   `UiMode` am I in?" — eliminating the race between "typed-ahead user
//!   message" and "approval answer" that a single-channel REPL has to
//!   heuristically resolve.
//! - State transitions that read top-to-bottom as one state machine,
//!   rather than as nested labelled loops.
//!
//! # Mid-turn user-message interjection
//!
//! The example exercises
//! [`LoopInterrupt::AfterToolResult`](agentkit_loop::LoopInterrupt::AfterToolResult).
//! User messages typed while a turn is in flight are buffered in
//! [`Mode::Driving`] and submitted to the driver at the next tool-round
//! boundary — without cancelling the turn.
//!
//! # Slash commands
//!
//! - `/exit`, `/quit`  — leave the REPL.
//! - `/cancel`         — abort the in-flight turn (also Ctrl-C).

use std::collections::HashMap;
use std::env;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentkit_compaction::{
    AgentBuilderCompactorExt, CompactionPipeline, CompactionReason, DropFailedToolResultsStrategy,
    DropReasoningStrategy, KeepRecentStrategy, StrategyCompactor,
};
use agentkit_core::{
    CancellationController, Delta, FinishReason, Item, ItemKind, MetadataMap, PartId, PartKind,
};
use agentkit_loop::{
    Agent, AgentEvent, InputRequest, LoopDriver, LoopError, LoopInterrupt, LoopObserver, LoopStep,
    ModelAdapter, ModelSession, ObservedEvent, PendingApproval, PromptCacheRequest,
    PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tools_core::{
    ApprovalDecision, ApprovalReason, ApprovalRequest, CommandPolicy, CompositePermissionChecker,
    PathPolicy, PermissionCode, PermissionDecision, PermissionDenial,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

const SYSTEM_PROMPT: &str = "\
You are a careful repository assistant operating inside a Claude-Code-style REPL.
Inspect the repository with the available fs.* and shell_exec tools instead of guessing.
Prefer concise answers. When using tools, use paths relative to the working directory.
If the user sends a new message while you're mid-task, integrate it into your plan for
the next step rather than restarting.
";

const DEFAULT_MAX_CONTEXT_TOKENS: u64 = 200_000;

// =============================================================================
// Channel types.
//
// `AgentCommand` — UI → agent.  The UI task decides what a raw stdin line
// means based on its local `UiMode`, then sends the appropriate typed
// command.  The agent task never inspects raw strings.
//
// `UiEvent` — agent → UI.  Carries forwarded `AgentEvent`s plus a small set
// of explicit transitions that tell the UI when to change mode or render
// the prompt.  The UI is purely reactive; it never polls the agent.
// =============================================================================

#[derive(Debug)]
enum AgentCommand {
    /// A user message destined for the model.  During a turn, buffered and
    /// flushed at the next tool-round boundary (or turn end).  At rest,
    /// starts a new turn.
    UserMessage(String),
    /// The user's answer to an outstanding approval prompt.  Only meaningful
    /// in [`Mode::AwaitingApproval`]; ignored otherwise.
    ApprovalAnswer(ApprovalDecision),
    /// Abort the in-flight turn (Ctrl-C or `/cancel`).  No-op if idle.
    Cancel,
    /// Quit the REPL (`/exit`, `/quit`, or stdin closed).
    Quit,
}

#[derive(Debug)]
enum UiEvent {
    /// A forwarded driver event.  The UI renders according to its own
    /// policy — streaming deltas, tool calls, usage footers, etc.
    Agent(AgentEvent),
    /// The driver is blocked on approval.  UI should switch to
    /// [`UiMode::ApprovalInput`] and render the approval prompt.
    ApprovalRequested(ApprovalRequest),
    /// The driver is at rest (no turn, no interrupt).  UI should render the
    /// message prompt and accept a new user message.
    Idle,
    /// The agent has started processing a user message.  UI uses this to
    /// decide whether to echo a `⎿ queued` ack when the user types
    /// additional messages before the turn finishes.
    Busy,
    /// The agent task has exited.  UI should drain any pending output and
    /// terminate.
    Shutdown,
}

// =============================================================================
// Token meter + compaction trigger (fires at 80% of context window).
//
// Shared between the agent task (TokenBudgetTrigger inspects current tokens
// to decide when to compact) and the UI task (Renderer displays the
// context-percentage footer).  All mutation is via atomics.
// =============================================================================

#[derive(Clone)]
struct TokenMeter {
    current: Arc<AtomicU64>,
    threshold: u64,
    max: u64,
}

impl TokenMeter {
    fn new(max: u64) -> Self {
        Self {
            current: Arc::new(AtomicU64::new(0)),
            threshold: max * 4 / 5,
            max,
        }
    }

    fn record(&self, total: u64) {
        self.current.store(total, Ordering::Relaxed);
    }

    fn reset(&self) {
        self.current.store(0, Ordering::Relaxed);
    }

    fn read(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }
}

fn token_budget_compactor(meter: TokenMeter) -> StrategyCompactor {
    StrategyCompactor::builder()
        .trigger(move |_transcript, _point| {
            if meter.read() >= meter.threshold {
                meter.reset();
                Some(CompactionReason::Custom("context-window-80pct".into()))
            } else {
                None
            }
        })
        .strategy(
            CompactionPipeline::new()
                .with_strategy(DropReasoningStrategy::new())
                .with_strategy(DropFailedToolResultsStrategy::new())
                .with_strategy(
                    KeepRecentStrategy::new(16)
                        .preserve_kind(ItemKind::System)
                        .preserve_kind(ItemKind::Context),
                ),
        )
        .build()
        .expect("token budget compactor")
}

// =============================================================================
// Observers.
//
// `MeterObserver` records token usage into the shared meter so the
// compaction trigger can read it without threading a cancellation-style
// handle all the way down.
//
// `ChannelObserver` forwards every AgentEvent to the UI task.  The channel
// is unbounded so a slow UI can never stall the driver (which would
// starve tool execution and model streaming).
// =============================================================================

struct MeterObserver {
    meter: TokenMeter,
}

impl LoopObserver for MeterObserver {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        if let AgentEvent::UsageUpdated(usage) = event
            && let Some(tokens) = usage.tokens.as_ref()
        {
            self.meter
                .record(tokens.input_tokens + tokens.output_tokens);
        }
    }
}

struct ChannelObserver {
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl LoopObserver for ChannelObserver {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        // If the UI has gone away we simply drop events — the agent task
        // will notice via its own channel and shut down on the next tick.
        let _ = self.tx.send(UiEvent::Agent(event));
    }
}

// =============================================================================
// UI task.
// =============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiMode {
    /// Stdin lines are user messages for the model.
    MessageInput,
    /// Stdin lines are approval answers (y/N or a deny reason).
    ApprovalInput,
}

struct Renderer {
    part_kinds: HashMap<PartId, PartKind>,
    streaming_text: bool,
    meter: TokenMeter,
}

impl Renderer {
    fn new(meter: TokenMeter) -> Self {
        Self {
            part_kinds: HashMap::new(),
            streaming_text: false,
            meter,
        }
    }

    fn end_text_stream(&mut self) {
        if self.streaming_text {
            println!();
            self.streaming_text = false;
        }
    }

    fn render(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ContentDelta(Delta::BeginPart { part_id, kind }) => {
                self.part_kinds.insert(part_id, kind);
            }
            AgentEvent::ContentDelta(Delta::AppendText { part_id, chunk }) => {
                if matches!(self.part_kinds.get(&part_id), Some(PartKind::Text)) {
                    self.streaming_text = true;
                    print!("{chunk}");
                    let _ = std::io::stdout().flush();
                }
            }
            // Buffered providers deliver the finished part in one shot via
            // CommitPart, without prior AppendText deltas. Render here so
            // the assistant reply is visible. For streaming providers,
            // AppendText has already printed the text, so we only close the
            // line.
            AgentEvent::ContentDelta(Delta::CommitPart { part }) => match part {
                agentkit_core::Part::Text(text) => {
                    if self.streaming_text {
                        self.end_text_stream();
                    } else {
                        println!("{}", text.text);
                    }
                }
                agentkit_core::Part::Reasoning(r) => {
                    self.end_text_stream();
                    if let Some(summary) = r.summary {
                        for line in summary.lines() {
                            println!("· {line}");
                        }
                    }
                }
                agentkit_core::Part::Structured(s) => {
                    self.end_text_stream();
                    println!("{}", s.value);
                }
                _ => {}
            },
            AgentEvent::ToolCallRequested(call) => {
                self.end_text_stream();
                let args =
                    serde_json::to_string(&call.input).unwrap_or_else(|_| call.input.to_string());
                println!("⏺ {}({})", call.name, truncate(&args, 160));
            }
            AgentEvent::MutationStarted { mutator, point, .. } => {
                self.end_text_stream();
                println!("✻ mutator {mutator} running at {point:?}…");
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
                println!("✻ mutator {mutator} finished (dirty={dirty}, replaced={replaced})");
            }
            AgentEvent::Warning { message } => {
                self.end_text_stream();
                eprintln!("⚠ {message}");
            }
            AgentEvent::RunFailed { message } => {
                self.end_text_stream();
                eprintln!("✗ {message}");
            }
            AgentEvent::TurnFinished(result) => {
                self.end_text_stream();
                if matches!(result.finish_reason, FinishReason::Cancelled) {
                    println!("— turn cancelled");
                }
                if let Some(tokens) = result.usage.as_ref().and_then(|u| u.tokens.as_ref()) {
                    let pct = self.meter.read() as f64 / self.meter.max.max(1) as f64 * 100.0;
                    println!(
                        "⟡ {} in · {} out · context {:.0}% of {}",
                        tokens.input_tokens, tokens.output_tokens, pct, self.meter.max
                    );
                }
            }
            _ => {}
        }
    }
}

/// UI task: owns stdin and stdout.  Reacts to `UiEvent`s from the agent,
/// classifies each stdin line by the current `UiMode`, emits
/// `AgentCommand`s.
async fn run_ui(
    cmd_tx: mpsc::Sender<AgentCommand>,
    mut evt_rx: mpsc::UnboundedReceiver<UiEvent>,
    meter: TokenMeter,
) {
    let mut mode = UiMode::MessageInput;
    // Tracks whether a turn is in flight; controls the `⎿ queued` echo for
    // typed-ahead user messages.  Kept in sync with the agent via
    // `UiEvent::Busy` / `UiEvent::Idle`.
    let mut busy = false;
    let mut renderer = Renderer::new(meter);

    // Dedicated stdin reader.  Lines arrive via a channel so the UI task
    // can select! between stdin and agent events.
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<Option<String>>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line_tx.send(Some(line)).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = line_tx.send(None);
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            biased;

            maybe_event = evt_rx.recv() => match maybe_event {
                Some(UiEvent::Agent(event)) => renderer.render(event),
                Some(UiEvent::ApprovalRequested(req)) => {
                    mode = UiMode::ApprovalInput;
                    println!();
                    println!("✻ approval required: {}", approval_label(&req.reason));
                    println!("  {}", req.summary);
                    print!("  approve? [y/N] (or type a deny reason): ");
                    let _ = std::io::stdout().flush();
                }
                Some(UiEvent::Idle) => {
                    mode = UiMode::MessageInput;
                    busy = false;
                    print!("\n› ");
                    let _ = std::io::stdout().flush();
                }
                Some(UiEvent::Busy) => {
                    busy = true;
                }
                Some(UiEvent::Shutdown) | None => return,
            },

            maybe_line = line_rx.recv() => match maybe_line {
                Some(Some(line)) => {
                    if let Some(cmd) = classify_line(&line, mode) {
                        // Flip UI mode eagerly when we send an approval
                        // answer; the agent will confirm with a subsequent
                        // Idle or a new ApprovalRequested if we get it
                        // wrong (we shouldn't, since UiMode is our own).
                        if matches!(cmd, AgentCommand::ApprovalAnswer(_)) {
                            mode = UiMode::MessageInput;
                        }
                        // Echo typed-ahead messages so the user sees that
                        // their input was registered while a turn is in
                        // flight.  At rest, the message just starts a new
                        // turn and needs no ack.
                        if busy
                            && let AgentCommand::UserMessage(text) = &cmd
                        {
                            println!("  ⎿ queued: {}", truncate(text.trim(), 140));
                            let _ = std::io::stdout().flush();
                        }
                        if cmd_tx.send(cmd).await.is_err() {
                            return;
                        }
                    } else {
                        // Empty line — render a fresh prompt if we're at
                        // the top level, otherwise ignore.
                        if mode == UiMode::MessageInput {
                            print!("› ");
                            let _ = std::io::stdout().flush();
                        }
                    }
                }
                Some(None) | None => {
                    // Stdin closed — ask the agent to shut down cleanly.
                    let _ = cmd_tx.send(AgentCommand::Quit).await;
                    return;
                }
            },
        }
    }
}

/// Classify a raw line into a typed command.  Slash commands are
/// mode-independent; everything else is interpreted by `mode`.
fn classify_line(line: &str, mode: UiMode) -> Option<AgentCommand> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed {
        "/exit" | "/quit" => return Some(AgentCommand::Quit),
        "/cancel" => return Some(AgentCommand::Cancel),
        _ => {}
    }
    match mode {
        UiMode::MessageInput => Some(AgentCommand::UserMessage(line.to_string())),
        UiMode::ApprovalInput => Some(AgentCommand::ApprovalAnswer(parse_approval(trimmed))),
    }
}

fn parse_approval(trimmed: &str) -> ApprovalDecision {
    match trimmed.to_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Approve,
        "n" | "no" => ApprovalDecision::Deny { reason: None },
        other => ApprovalDecision::Deny {
            reason: Some(other.to_string()),
        },
    }
}

fn approval_label(reason: &ApprovalReason) -> &'static str {
    match reason {
        ApprovalReason::PolicyRequiresConfirmation => "policy requires confirmation",
        ApprovalReason::EscalatedRisk => "escalated risk",
        ApprovalReason::UnknownTarget => "unknown target",
        ApprovalReason::SensitivePath => "sensitive path",
        ApprovalReason::SensitiveCommand => "sensitive command",
        ApprovalReason::SensitiveServer => "sensitive MCP server",
        ApprovalReason::SensitiveAuthScope => "sensitive auth scope",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

// =============================================================================
// Agent task.
// =============================================================================

/// Agent-task state machine.
///
/// - `Idle`: no turn in flight.  Block on the command channel for a
///   `UserMessage`.  Any other command (`Cancel`, stray `ApprovalAnswer`)
///   is a no-op here.
///
/// - `Driving`: a turn is in flight.  Race `driver.next()` against the
///   command channel.  User messages accumulate in `buffered` and are
///   flushed at the next `AfterToolResult` yield or at turn end.  `Cancel`
///   interrupts the driver; `Quit` interrupts and then exits.
///
/// - `AwaitingApproval`: driver returned `ApprovalRequest` and is paused.
///   No `next()` in flight — we can mutate the driver freely.  Block on
///   the command channel for `ApprovalAnswer` (or fold stray user
///   messages into the preserved `buffered` list).
enum Mode {
    Idle {
        input: InputRequest,
    },
    Driving {
        buffered: Vec<Item>,
    },
    AwaitingApproval {
        pending: PendingApproval,
        buffered: Vec<Item>,
    },
}

/// Agent task: owns the `LoopDriver` exclusively.  Every path that mutates
/// the driver runs from here, so the `&mut` borrow rule is a local concern.
///
/// The driver is started inside this task (rather than passed in already-
/// started) so we don't pay for a session before the user has anything to
/// say. We wait for `cmd_rx` to deliver the first `UserMessage`, preload it
/// as the builder's `input`, then start the driver and enter the driving
/// loop.
async fn run_agent<M>(
    agent_builder: agentkit_loop::AgentBuilder<M>,
    session_config: SessionConfig,
    cancellation: CancellationController,
    mut cmd_rx: mpsc::Receiver<AgentCommand>,
    evt_tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<(), LoopError>
where
    M: ModelAdapter,
    M::Session: ModelSession + Send,
{
    let _ = evt_tx.send(UiEvent::Idle);
    // Wait for the first real user message before paying for a session.
    // Cancel/ApprovalAnswer at rest are stale UI state — drop them.
    let first_text = loop {
        match cmd_rx.recv().await {
            Some(AgentCommand::UserMessage(text)) => break text,
            Some(AgentCommand::Quit) | None => return Ok(()),
            Some(AgentCommand::Cancel) | Some(AgentCommand::ApprovalAnswer(_)) => continue,
        }
    };

    let agent = agent_builder
        .transcript(vec![Item::text(ItemKind::System, SYSTEM_PROMPT)])
        .input(vec![Item::text(ItemKind::User, first_text)])
        .build()?;

    let mut driver = agent.start(session_config).await?;

    let _ = evt_tx.send(UiEvent::Busy);

    let mut mode = Mode::Driving {
        buffered: Vec::new(),
    };

    loop {
        mode = match mode {
            Mode::Idle { input } => {
                let _ = evt_tx.send(UiEvent::Idle);
                match cmd_rx.recv().await {
                    Some(AgentCommand::UserMessage(text)) => {
                        input.submit(&mut driver, vec![Item::text(ItemKind::User, text)])?;
                        let _ = evt_tx.send(UiEvent::Busy);
                        Mode::Driving {
                            buffered: Vec::new(),
                        }
                    }
                    Some(AgentCommand::Quit) | None => break,
                    // Cancel at rest: nothing to cancel.  Stray approval
                    // answer: the UI is out of sync — drop it.
                    Some(AgentCommand::Cancel) | Some(AgentCommand::ApprovalAnswer(_)) => {
                        Mode::Idle { input }
                    }
                }
            }

            Mode::Driving { mut buffered } => {
                tokio::select! {
                    biased;

                    step = driver.next() => match step? {
                        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(info)) => {
                            if !buffered.is_empty() {
                                info.submit(&mut driver, std::mem::take(&mut buffered))?;
                            }
                            Mode::Driving { buffered }
                        }
                        LoopStep::Finished(_) => Mode::Driving { buffered },
                        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => {
                            if !buffered.is_empty() {
                                req.submit(&mut driver, std::mem::take(&mut buffered))?;
                                Mode::Driving { buffered }
                            } else {
                                Mode::Idle { input: req }
                            }
                        }
                        LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                            let _ = evt_tx.send(UiEvent::ApprovalRequested(pending.request.clone()));
                            Mode::AwaitingApproval { pending, buffered }
                        }
                    },

                    maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                        Some(AgentCommand::UserMessage(text)) => {
                            buffered.push(Item::text(ItemKind::User, text));
                            Mode::Driving { buffered }
                        }
                        Some(AgentCommand::Cancel) => {
                            cancellation.interrupt();
                            Mode::Driving { buffered }
                        }
                        Some(AgentCommand::Quit) | None => {
                            cancellation.interrupt();
                            // Drain the in-flight turn so the driver
                            // returns cleanly; don't re-enter after that.
                            drain_turn(&mut driver).await?;
                            break;
                        }
                        // Stray approval answer while driving (UI was
                        // still in approval mode for an already-resolved
                        // prompt, or out-of-order).  Drop it.
                        Some(AgentCommand::ApprovalAnswer(_)) => Mode::Driving { buffered },
                    },
                }
            }

            Mode::AwaitingApproval {
                pending,
                mut buffered,
            } => match cmd_rx.recv().await {
                Some(AgentCommand::ApprovalAnswer(decision)) => {
                    match decision {
                        ApprovalDecision::Approve => pending.approve(&mut driver)?,
                        ApprovalDecision::Deny { reason: None } => pending.deny(&mut driver)?,
                        ApprovalDecision::Deny {
                            reason: Some(reason),
                        } => pending.deny_with_reason(&mut driver, reason)?,
                    }
                    Mode::Driving { buffered }
                }
                // User typed a message instead of an answer — preserve it
                // for after the approval resolves, then keep waiting.
                Some(AgentCommand::UserMessage(text)) => {
                    buffered.push(Item::text(ItemKind::User, text));
                    Mode::AwaitingApproval { pending, buffered }
                }
                Some(AgentCommand::Cancel) => {
                    pending.deny_with_reason(&mut driver, "user cancelled the turn")?;
                    cancellation.interrupt();
                    Mode::Driving { buffered }
                }
                Some(AgentCommand::Quit) | None => {
                    pending.deny_with_reason(&mut driver, "user requested quit")?;
                    cancellation.interrupt();
                    drain_turn(&mut driver).await?;
                    break;
                }
            },
        };
    }

    let _ = evt_tx.send(UiEvent::Shutdown);
    Ok(())
}

/// Drive `next()` to a non-cooperative terminal state.  Used during
/// shutdown so the driver cleans up its in-flight work after we've
/// signalled cancellation.
async fn drain_turn<S>(driver: &mut LoopDriver<S>) -> Result<(), LoopError>
where
    S: ModelSession + Send,
{
    loop {
        match driver.next().await? {
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            _ => return Ok(()),
        }
    }
}

// =============================================================================
// main — wire the tasks together.
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let config = OpenRouterConfig::from_env()?;
    let model_name = config.model.clone();
    let adapter = OpenRouterAdapter::new(config)?;

    let tools = agentkit_tool_fs::registry().merge(agentkit_tool_shell::registry());

    let workspace_root = env::current_dir()?;
    let permissions = CompositePermissionChecker::new(PermissionDecision::Deny(PermissionDenial {
        code: PermissionCode::UnknownRequest,
        message: "tool request is not covered by any policy".into(),
        metadata: MetadataMap::new(),
    }))
    .with_policy(
        PathPolicy::new()
            .allow_root(workspace_root.clone())
            .require_approval_outside_allowed(true),
    )
    .with_policy(
        CommandPolicy::new()
            .allow_cwd(workspace_root.clone())
            .require_approval_for_unknown(true),
    );

    let max_ctx = env::var("AGENTKIT_MAX_CONTEXT_TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS);
    let meter = TokenMeter::new(max_ctx);

    let cancellation = CancellationController::new();

    // UI ← agent: high-frequency events (streaming deltas). Unbounded so a
    // slow UI can never stall the driver.
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<UiEvent>();
    // UI → agent: low-frequency human input. Bounded (natural backpressure
    // if the agent falls behind, which it shouldn't).
    let (cmd_tx, cmd_rx) = mpsc::channel::<AgentCommand>(64);

    let agent_builder = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .permissions(permissions)
        .cancellation(cancellation.handle())
        .observer(MeterObserver {
            meter: meter.clone(),
        })
        .observer(ChannelObserver { tx: evt_tx.clone() })
        .compactor(token_budget_compactor(meter.clone()));

    let session_config = SessionConfig::new("openrouter-coding-agent")
        .with_cache(PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short));

    print_banner(&model_name, &workspace_root, max_ctx);

    // Ctrl-C → synthesize a Cancel command, so signal handling follows the
    // same command path as typed input.
    {
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break;
                }
                if cmd_tx.send(AgentCommand::Cancel).await.is_err() {
                    break;
                }
            }
        });
    }

    // Agent task runs in the background; UI task runs in the foreground so
    // that stdin / stdout see the same terminal as the user.
    let agent_handle = tokio::spawn(run_agent(
        agent_builder,
        session_config,
        cancellation,
        cmd_rx,
        evt_tx,
    ));
    run_ui(cmd_tx, evt_rx, meter).await;

    match agent_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err.into()),
        Err(join_err) => Err(join_err.into()),
    }
}

fn print_banner(model: &str, root: &Path, max_ctx: u64) {
    println!("openrouter-coding-agent  ({model})");
    println!("cwd: {}", root.display());
    println!("context: {max_ctx} tokens · compaction at 80%");
    println!("Ctrl-C cancels the current turn · /exit quits · type ahead to queue input");
}
