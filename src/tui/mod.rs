//! The terminal client.
//!
//! The client owns a `kit serve` child process and talks ACP to it over that
//! process's stdio, so the UI runs against exactly the protocol surface any
//! other editor would use. The child's stderr carries two things: ordinary
//! diagnostics, shown in the log pane, and the runtime side channel
//! ([`crate::events`]) that feeds the live graph of a running Runlet program.

mod app;
mod editor;
mod markdown;
mod plan;
mod theme;
mod ui;
mod wrap;

use std::{path::Path, process::Stdio, time::Duration};

use agent_client_protocol::{ByteStreams, schema::ProtocolVersion};
use agentkit_acp::{
    CancelNotification, ContentBlock, SessionNotification, SessionUpdate, ToolCallContent,
};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
};
use futures_util::{FutureExt, StreamExt};
use ratatui::DefaultTerminal;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::events::{self, EVENTS_ENV};

use app::{Action, App, Update};

/// Animation and elapsed-time refresh interval.
const TICK: Duration = Duration::from_millis(90);
/// Terminal events applied per frame, so a paste lands in one redraw.
const MAX_BURST: usize = 4_096;
/// Output lines kept per tool call; the fold shows the count either way.
const MAX_OUTPUT_LINES: usize = 5_000;

pub async fn run(root: &Path, model: &str, a2a: &str) -> Result<(), Box<dyn std::error::Error>> {
    // The agent fixes itself to the canonical root, so the client resolves it
    // up front: the header names a real directory and the ACP session opens on
    // the same path the agent accepts.
    let root = &root.canonicalize()?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--model")
        .arg(model)
        .arg("--a2a")
        .arg(a2a)
        .env(EVENTS_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().ok_or("could not open Kit stdin")?;
    let stdout = child.stdout.take().ok_or("could not open Kit stdout")?;
    let stderr = child.stderr.take().ok_or("could not open Kit stderr")?;
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let root = root.to_path_buf();
    let model = model.to_string();
    let a2a = a2a.to_string();
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();

    let diagnostics = updates_tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let update = match events::parse(&line) {
                Some(event) => Update::Runtime(event),
                None => Update::Log(line),
            };
            if diagnostics.send(update).is_err() {
                return;
            }
        }
        // Stderr closing means the agent process is gone; stop any spinner
        // waiting on a turn that can no longer finish.
        let _ = diagnostics.send(Update::TurnEnded(Some(
            "the agent process exited — press ctrl+c to leave".into(),
        )));
    });

    let notifications = updates_tx.clone();
    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                for update in translate(notification) {
                    let _ = notifications.send(update);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |connection| {
            connection
                .send_request(agentkit_acp::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(agentkit_acp::NewSessionRequest::new(root.clone()))
                .block_task()
                .await?;
            let session_id = session.session_id.clone();

            let mut terminal =
                enter().map_err(agent_client_protocol::Error::into_internal_error)?;
            let mut app = App::new(root, model, a2a);
            let mut events = EventStream::new();
            let mut ticker = tokio::time::interval(TICK);
            let result = async {
                loop {
                    terminal
                        .draw(|frame| ui::draw(frame, &mut app))
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    tokio::select! {
                        terminal_event = events.next() => {
                            // A paste is a burst: one bracketed-paste event, or
                            // thousands of key events where the terminal cannot
                            // bracket it. Applying everything the terminal has
                            // already buffered keeps a paste to a single redraw
                            // instead of one frame per character.
                            let mut next = terminal_event;
                            let mut action = Action::None;
                            for _ in 0..MAX_BURST {
                                match next {
                                    Some(Ok(event)) => action = handle(&mut app, event),
                                    Some(Err(_)) | None => return Ok(()),
                                }
                                if !matches!(action, Action::None) {
                                    break;
                                }
                                match events.next().now_or_never() {
                                    Some(buffered) => next = buffered,
                                    None => break,
                                }
                            }
                            match action {
                                Action::Quit => return Ok(()),
                                Action::Submit(prompt) => {
                                    app.push_user(prompt.clone());
                                    let connection = connection.clone();
                                    let session = session_id.clone();
                                    let updates = updates_tx.clone();
                                    tokio::spawn(async move {
                                        let outcome = connection
                                            .send_request(agentkit_acp::PromptRequest::new(
                                                session,
                                                vec![ContentBlock::Text(
                                                    agentkit_acp::TextContent::new(prompt),
                                                )],
                                            ))
                                            .block_task()
                                            .await;
                                        let _ = updates.send(Update::TurnEnded(match outcome {
                                            Ok(_) => None,
                                            Err(error) => Some(error.to_string()),
                                        }));
                                    });
                                }
                                Action::Cancel => {
                                    let _ = connection.send_notification(
                                        CancelNotification::new(session_id.clone()),
                                    );
                                }
                                Action::None => {}
                            }
                        },
                        update = updates_rx.recv() => match update {
                            Some(update) => {
                                app.apply(update);
                                while let Ok(update) = updates_rx.try_recv() {
                                    app.apply(update);
                                }
                            }
                            None => return Ok(()),
                        },
                        _ = ticker.tick() => app.tick = app.tick.wrapping_add(1),
                    }
                }
            }
            .await;
            leave(terminal);
            let _ = child.kill().await;
            result
        })
        .await?;
    Ok(())
}

/// Applies one terminal event, returning the work it asks for.
fn handle(app: &mut App, event: Event) -> Action {
    match event {
        Event::Key(key) => app.handle_key(key),
        Event::Mouse(mouse) => {
            app.handle_mouse(mouse);
            Action::None
        }
        Event::Paste(text) => {
            app.paste(&text);
            Action::None
        }
        _ => Action::None,
    }
}

fn enter() -> std::io::Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    let mut stdout = std::io::stdout();
    // Bracketed paste is what keeps pasted text out of the key stream: without
    // it every newline in a paste arrives as a return press, which submits the
    // prompt part-way through.
    let _ = execute!(stdout, EnableBracketedPaste, EnableMouseCapture);
    // Kitty-protocol terminals report `cmd`, `shift+enter`, and key release
    // separately; the client falls back to control keys where they do not.
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    Ok(terminal)
}

fn leave(terminal: DefaultTerminal) {
    let mut stdout = std::io::stdout();
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout, DisableMouseCapture, DisableBracketedPaste);
    drop(terminal);
    ratatui::restore();
}

/// Maps one ACP session notification onto client updates.
fn translate(notification: SessionNotification) -> Vec<Update> {
    match notification.update {
        SessionUpdate::AgentMessageChunk(chunk) => text_of(chunk.content)
            .map(Update::Text)
            .into_iter()
            .collect(),
        SessionUpdate::AgentThoughtChunk(chunk) => text_of(chunk.content)
            .map(Update::Thought)
            .into_iter()
            .collect(),
        SessionUpdate::ToolCall(call) => vec![Update::ToolStarted {
            id: call.tool_call_id.to_string(),
            title: call.title,
            kind: call.kind,
            script: call.raw_input.as_ref().and_then(script_of),
        }],
        SessionUpdate::ToolCallUpdate(update) => vec![Update::ToolUpdated {
            id: update.tool_call_id.to_string(),
            status: update.fields.status,
            script: update.fields.raw_input.as_ref().and_then(script_of),
            output: output_of(update.fields.content.as_deref()),
        }],
        SessionUpdate::UsageUpdate(usage) => vec![Update::Usage {
            used: usage.used,
            size: usage.size,
        }],
        _ => Vec::new(),
    }
}

fn text_of(content: ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text),
        _ => None,
    }
}

/// The Runlet program inside a `compose` call's input, when there is one.
fn script_of(input: &Value) -> Option<String> {
    input
        .get("script")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A tool call's output as readable lines, kept whole for the folded card.
fn output_of(content: Option<&[ToolCallContent]>) -> Vec<String> {
    let mut output = Vec::new();
    for entry in content.unwrap_or_default() {
        match entry {
            ToolCallContent::Content(content) => {
                if let Some(text) = text_of(content.content.clone()) {
                    output.extend(readable(&text));
                }
            }
            ToolCallContent::Diff(diff) => output.push(format!(
                "{} · {} lines",
                diff.path.display(),
                diff.new_text.lines().count()
            )),
            _ => {}
        }
        if output.len() >= MAX_OUTPUT_LINES {
            break;
        }
    }
    output.truncate(MAX_OUTPUT_LINES);
    output
}

/// Splits tool output into display lines.
///
/// Structured results arrive as JSON, so a bare string unwraps to its own text
/// — otherwise the card shows one endless line full of `\n` escapes — and an
/// object or array is pretty-printed.
fn readable(text: &str) -> Vec<String> {
    let rendered = match serde_json::from_str::<Value>(text) {
        Ok(Value::String(inner)) => inner,
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    };
    rendered
        .lines()
        .map(|line| line.replace('\t', "    "))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::readable;

    #[test]
    fn unwraps_json_string_results_into_real_lines() {
        assert_eq!(
            readable("\"## Overview\\n\\nKit is small.\""),
            ["## Overview", "", "Kit is small."]
        );
    }

    #[test]
    fn pretty_prints_structured_results() {
        assert_eq!(
            readable("{\"exit_code\":0}"),
            ["{", "  \"exit_code\": 0", "}"]
        );
    }

    #[test]
    fn leaves_plain_text_alone() {
        assert_eq!(readable("one\ntwo"), ["one", "two"]);
    }
}
