# The interactive CLI

This chapter covers the host-side implementation of an interactive coding agent CLI: architecture, input handling, output rendering, approval UX, session lifecycle, and error recovery.

Everything in this chapter is host code — agentkit doesn't include a CLI. The library provides the loop, and the host provides the user interface. This separation means the same agentkit crates power a terminal CLI, a web server, an IDE plugin, or a headless CI agent.

## Architecture: actor + command channel

An interactive CLI has two responsibilities that pull in different directions:

- **Driving the loop** — owning the single `&mut LoopDriver`, processing `LoopStep`s, holding in-flight transcript state.
- **Owning the terminal** — reading stdin, rendering streaming output, switching between message-input and approval-input modes.

A single-task design solves this with `tokio::select!` over `driver.next()` and a stdin reader, plus local buffering for messages typed mid-turn. That works, but it tangles "when is the driver at rest?" with "when should I render a prompt?". The production pattern for non-trivial CLIs is to split the two concerns into separate tasks communicating via typed channels:

```text
  ┌──────────────┐   AgentCommand    ┌───────────────────┐
  │   UI task    │ ────────────────▶ │   Agent task      │
  │ (stdin/TTY)  │                   │ (owns LoopDriver) │
  │              │ ◀──────────────── │                   │
  └──────────────┘     UiEvent       └───────────────────┘
```

- **Agent task** owns the `LoopDriver`. Runs a `Mode::Idle` / `Driving` / `AwaitingApproval` state machine driven by incoming `AgentCommand`s and outgoing `LoopStep`s. Knows nothing about terminal rendering.
- **UI task** owns stdin and stdout. Holds a local `UiMode::{MessageInput, ApprovalInput}` that determines how each typed line is classified. Knows nothing about the driver.
- **Observers** run inside the agent task. A `ChannelObserver` forwards every `AgentEvent` to the UI as a `UiEvent::Agent(event)`; the UI renders from that stream.

Two typed channels:

```rust
// UI → agent.  The UI decides what a raw stdin line means based on its
// local UiMode; the agent never inspects raw strings.
enum AgentCommand {
    UserMessage(String),
    ApprovalAnswer(ApprovalDecision),
    Cancel,
    Quit,
}

// agent → UI.  Carries forwarded AgentEvents plus explicit transitions
// that tell the UI when to change mode or render the prompt.
enum UiEvent {
    Agent(AgentEvent),
    ApprovalRequested(ApprovalRequest),  // UI switches to ApprovalInput
    Idle,                                // UI renders prompt, switches to MessageInput
    Busy,                                // UI shows `⎿ queued` on mid-turn typing
    Shutdown,
}
```

Why this shape:

- **The `&mut LoopDriver` invariant stays intact.** Every driver mutation happens inside the agent task. The UI task cannot accidentally submit input while `next()` is awaiting.
- **The approval race disappears.** With one string channel both typed-ahead user messages and approval answers arrive as `AgentCommand`, and a heuristic must decide which is which. With two typed variants and a UI-owned `UiMode`, the UI classifies at the source — no race.
- **The front-end is pluggable.** Swap the UI task for an HTTP handler, a test harness, or a GUI without touching the agent code.
- **State transitions read top-to-bottom.** The agent task is a self-contained `loop { mode = match mode { … } }` state machine.

The `openrouter-coding-agent` example implements this pattern end-to-end.

## The agent-task state machine

```rust
enum Mode {
    Idle { input: InputRequest },
    Driving { buffered: Vec<Item> },
    AwaitingApproval { pending: PendingApproval, buffered: Vec<Item> },
}

// The agent was built with `.transcript(system + context)` and
// `.input(first_user_message)`, so the first `next()` dispatches the
// model directly. After the opening turn, subsequent user messages flow
// through the `InputRequest`/`ToolRoundInfo` handles below.

loop {
    mode = match mode {
        Mode::Idle { input } => {
            evt_tx.send(UiEvent::Idle).ok();
            match cmd_rx.recv().await {
                Some(AgentCommand::UserMessage(text)) => {
                    input.submit(&mut driver, vec![Item::text(ItemKind::User, text)])?;
                    evt_tx.send(UiEvent::Busy).ok();
                    Mode::Driving { buffered: Vec::new() }
                }
                Some(AgentCommand::Quit) | None => break,
                _ => Mode::Idle { input },  // stray commands at rest: drop
            }
        }

        Mode::Driving { mut buffered } => tokio::select! {
            step = driver.next() => match step? {
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(info)) => {
                    // Flush typed-ahead messages so the next model call sees them.
                    if !buffered.is_empty() {
                        info.submit(&mut driver, std::mem::take(&mut buffered))?;
                    }
                    Mode::Driving { buffered }
                }
                LoopStep::Finished(_) => Mode::Driving { buffered },
                LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => {
                    if !buffered.is_empty() {
                        req.submit(&mut driver, std::mem::take(&mut buffered))?;
                        Mode::Driving { buffered }  // auto-advance
                    } else {
                        Mode::Idle { input: req }
                    }
                }
                LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                    evt_tx.send(UiEvent::ApprovalRequested(pending.request.clone())).ok();
                    Mode::AwaitingApproval { pending, buffered }
                }
            },
            Some(cmd) = cmd_rx.recv() => match cmd {
                AgentCommand::UserMessage(text) => {
                    buffered.push(Item::text(ItemKind::User, text));
                    Mode::Driving { buffered }
                }
                AgentCommand::Cancel => {
                    cancellation.interrupt();
                    Mode::Driving { buffered }
                }
                AgentCommand::Quit | _ => { /* cancel + drain + break */ }
            },
        },

        Mode::AwaitingApproval { pending, mut buffered } =>
            match cmd_rx.recv().await {
                Some(AgentCommand::ApprovalAnswer(decision)) => {
                    apply_approval(pending, decision, &mut driver)?;
                    Mode::Driving { buffered }
                }
                Some(AgentCommand::UserMessage(text)) => {
                    // User typed a message during the approval — preserve it.
                    buffered.push(Item::text(ItemKind::User, text));
                    Mode::AwaitingApproval { pending, buffered }
                }
                Some(AgentCommand::Cancel) => {
                    pending.deny_with_reason(&mut driver, "cancelled")?;
                    cancellation.interrupt();
                    Mode::Driving { buffered }
                }
                _ => break,
            },
    };
}
```

`InputRequest` and `ToolRoundInfo` both consume themselves on `submit`, so the typed handle in scope at each yield point is the only way to push input — there is no alternative entry point that could race with `next()`.

The three modes correspond to the three reasons a host might be waiting: waiting for the user to speak, waiting for the driver to finish work, waiting for the user to answer an approval.

## The UI-task classifier

```rust
enum UiMode { MessageInput, ApprovalInput }

fn classify_line(line: &str, mode: UiMode) -> Option<AgentCommand> {
    let trimmed = line.trim();
    if trimmed.is_empty() { return None; }
    match trimmed {
        "/exit" | "/quit" => return Some(AgentCommand::Quit),
        "/cancel"         => return Some(AgentCommand::Cancel),
        _ => {}
    }
    match mode {
        UiMode::MessageInput  => Some(AgentCommand::UserMessage(line.to_string())),
        UiMode::ApprovalInput => Some(AgentCommand::ApprovalAnswer(parse_answer(trimmed))),
    }
}
```

The UI flips to `ApprovalInput` on `UiEvent::ApprovalRequested` and back to `MessageInput` when it sends an `ApprovalAnswer` (or on the next `UiEvent::Idle`). Slash commands are mode-independent. The `Busy` event tracks whether a `⎿ queued` echo should accompany a `UserMessage` — useful so typed-ahead messages visibly reach the classifier even when streaming output is interleaving with them on the terminal.

## Minimal skeleton

For simple CLIs or prototypes, a single-task skeleton with cooperative-yield passthrough is still fine. Preload the system prompt and the user input on the builder so the first `next()` dispatches the model directly:

```rust
let agent = Agent::builder()
    .model(adapter)
    .transcript(vec![system_item])
    .input(vec![user_item(&input)])
    .build()?;

let mut driver = agent.start(session_config).await?;

loop {
    match driver.next().await? {
        LoopStep::Finished(_) => break,
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => break,
        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
        LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(p)) => handle_approval(p, &mut driver)?,
    }
}
```

This is what the one-shot CLIs in the repo (`openrouter-chat`, `openrouter-agent-cli`) use. The actor split is worth adopting when a CLI grows mid-turn interjection, rich approval UX, or a pluggable front-end.

## Input handling

A coding agent CLI needs to handle:

- Single-line user messages
- Multi-line input (pasted code, heredocs)
- Special commands (exit, help, clear)
- Ctrl-C for turn cancellation (not process exit)

### Cancellation wiring

Wire Ctrl-C to the `CancellationController`, not to process exit:

```rust
let controller = CancellationController::new();
let handle = controller.handle();

ctrlc::set_handler(move || {
    controller.interrupt();
})?;
```

The first Ctrl-C cancels the current turn. The model sees `FinishReason::Cancelled` and the turn ends cleanly. The second Ctrl-C (if nothing is running) exits the process.

## Output rendering

### Streaming text

The `StdoutReporter` renders `ContentDelta` events as they arrive. For a CLI, this means writing each text chunk to stdout immediately:

```rust
fn handle_event(&self, event: ObservedEvent) {
    if let AgentEvent::ContentDelta(Delta::AppendText { chunk, .. }) = event.event {
        print!("{}", chunk);
        std::io::stdout().flush().ok();
    }
}
```

### Tool activity

Display tool calls as they happen so the user knows what the agent is doing:

```
→ fs_read_file(path: "src/main.rs")
→ fs_replace_in_file(path: "src/main.rs", ...)
→ shell_exec(executable: "cargo", argv: ["build"])
```

### Usage reporting

At the end of each turn, display token counts and cost:

```
tokens: 1,234 in / 567 out | cost: $0.02
```

## Approval UX

When the loop returns an approval interrupt, present it clearly:

```
⚠ shell_exec wants to run: rm -rf target/
  Allow? [y/n/always]:
```

Consider supporting:

- `y` — approve once
- `n` — deny
- `always` — approve and add to allowlist for this session

The approval response maps to `ApprovalDecision::Approve` or `ApprovalDecision::Deny`.

## Session lifecycle

### Multi-turn sessions

A coding agent session typically spans many user turns. The driver persists across turns — the transcript accumulates, compaction fires as needed, and the model retains context from earlier in the conversation.

### Graceful shutdown

On exit, flush any buffered reporters, print a final usage summary, and clean up resources. If MCP servers are connected, shut them down cleanly.

## Error recovery

### Model errors

If the model returns an error (rate limit, content filter, network failure), the driver returns `Err(LoopError::...)`. Display the error and let the user decide:

```rust
match driver.next().await {
    Ok(step) => handle_step(step),
    Err(LoopError::Provider(msg)) => {
        eprintln!("Model error: {msg}");
        eprintln!("Press Enter to retry, or type a new message:");
        // Don't exit — the session is still valid
    }
    Err(LoopError::Cancelled) => {
        eprintln!("Turn cancelled.");
        // Session is still valid, user can send another message
    }
    Err(e) => {
        eprintln!("Fatal error: {e}");
        break;  // Only exit on truly unrecoverable errors
    }
}
```

The key insight: most errors are recoverable. A rate limit resolves after waiting. A content filter can be worked around by rephrasing. A network timeout may succeed on retry. Only exit the session on errors that genuinely corrupt the driver state.

### Tool errors

Tool failures are returned to the model as a `ToolResultPart` with `is_error: true`. The model sees the error message and can decide to retry, try a different approach, or report the failure. The CLI doesn't need to handle tool errors specially — they're part of the normal conversation flow.

```text
Tool error flow (handled entirely within the loop):

  Model: ToolCall(fs_read_file, { path: "main.rs" })
  Tool:  ToolResultPart { is_error: true, output: "File not found" }
  Model: "The file doesn't exist in the current directory. Let me check..."
  Model: ToolCall(shell_exec, { executable: "find", argv: [".", "-name", "main.rs"] })
  Tool:  ToolResultPart { output: "./src/main.rs" }
  Model: ToolCall(fs_read_file, { path: "./src/main.rs" })
  Tool:  ToolResultPart { output: "fn main() { ... }" }

  The host never saw the error — the model handled it autonomously.
```

## Design checklist

A production interactive CLI should handle all of these:

- [ ] Ctrl-C cancels the current turn, not the process
- [ ] Second Ctrl-C (when no turn is running) exits cleanly
- [ ] Streaming text renders as it arrives
- [ ] Tool calls are displayed with name and key arguments
- [ ] Approval prompts clearly show what's being requested
- [ ] Usage is displayed after each turn
- [ ] Model errors are displayed and the session continues
- [ ] Graceful shutdown flushes reporters and disconnects MCP
- [ ] Multi-line input is supported for pasting code

> **Example:** [`openrouter-agent-cli`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-agent-cli) implements most of these patterns. The remaining work for a production CLI is polish: better terminal rendering, richer approval UX, and configuration management.
