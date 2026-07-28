# `openrouter-coding-agent`

Claude-Code-style REPL on top of the agentkit loop. The primary purpose of this example is to demonstrate the **actor / command-channel** pattern for interactive agents — the production shape that keeps the driver's `&mut self` invariant intact while separating terminal I/O from loop orchestration.

## Architecture

```text
  ┌──────────────┐   AgentCommand    ┌───────────────────┐
  │   UI task    │ ────────────────▶ │   Agent task      │
  │ (stdin/TTY)  │                   │ (owns LoopDriver) │
  │              │ ◀──────────────── │                   │
  └──────────────┘     UiEvent       └───────────────────┘
```

- **Agent task** (`run_agent`) — owns the single `&mut LoopDriver`. Runs a `Mode::Idle` / `Driving` / `AwaitingApproval` state machine driven by incoming `AgentCommand`s and outgoing `LoopStep`s. Knows nothing about terminal rendering.
- **UI task** (`run_ui`) — owns stdin and stdout. Holds a local `UiMode::{MessageInput, ApprovalInput}` that decides how each typed line is classified. Knows nothing about the driver.
- **Observers** — a `ChannelObserver` forwards every `AgentEvent` to the UI for rendering; a `MeterObserver` records token usage into the shared `TokenMeter` used by the compaction trigger. Both live in the agent task and are stacked via `.observer(...)` calls on the builder.

Two typed channels:

```rust
enum AgentCommand { UserMessage(String), ApprovalAnswer(ApprovalDecision), Cancel, Quit }
enum UiEvent      { Agent(AgentEvent), ApprovalRequested(ApprovalRequest), Idle, Busy, Shutdown }
```

The key properties this gives up-stack:

- **No race on approval.** The UI's own `UiMode` decides whether a typed line is a user message or an approval answer — the agent never has to guess.
- **Front-end swappability.** Replace `run_ui` with an HTTP handler, a test harness, or a GUI shell without touching `run_agent`.
- **Clean state transitions.** The agent is one `loop { mode = match mode { … } }` state machine — no nested labelled loops, no `tokio::select!` in the top-level control flow.

## Loop features exercised

- **Mid-turn user-message interjection** via `LoopInterrupt::AfterToolResult`. Messages typed during a turn are buffered in `Mode::Driving` and flushed into the transcript at the next tool-round boundary, without cancelling the turn.
- **Tool-round cancellation** via `CancellationController`. `/cancel` and Ctrl-C both go through `AgentCommand::Cancel`; the agent interrupts the active turn.
- **Context-window compaction** via a `TokenMeter`-backed trigger that fires at 80% of the configured context window.
- **OpenRouter streaming delta rendering.** The observer prints `Delta::AppendText` chunks as they arrive and falls back to `Delta::CommitPart` for buffered providers.

## Run

```bash
cargo run -p openrouter-coding-agent
```

You get a `›` prompt. Type a message and press enter.

- Type while the agent is mid-turn (e.g. during a slow `shell_exec`): the line is echoed as `⎿ queued` and reaches the model at the next tool-round boundary. The turn itself is not cancelled.
- `Ctrl-C` cancels the in-flight turn. The next prompt appears.
- `/cancel` cancels the turn without quitting (same as `Ctrl-C` but via the command channel).
- `/exit` or `/quit` quits — at the idle prompt, immediately; during a turn, after the current turn's cancellation settles.

## Permissions

- Filesystem access inside the current working directory is allowed; paths outside request interactive approval.
- Shell commands with cwd inside the working directory are allowed; unknown executables request approval.
- Approval prompts: `y`/`yes` to approve, `n`/`no` to deny, or type a sentence to deny with that reason.

## Compaction

Fires when reported `input_tokens + output_tokens` reach 80% of the configured context window (default 200k). Pipeline drops reasoning, drops failed tool results, then keeps the 16 most recent items while preserving `System` and `Context` items.

Override the context size with `AGENTKIT_MAX_CONTEXT_TOKENS`:

```bash
AGENTKIT_MAX_CONTEXT_TOKENS=128000 cargo run -p openrouter-coding-agent
```

## Environment

Loads from the workspace `.env`. Requires `OPENROUTER_API_KEY` and `OPENROUTER_MODEL`.
