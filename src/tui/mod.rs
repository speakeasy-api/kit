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

use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{ByteStreams, schema::ProtocolVersion};
use agentkit_acp::{
    CancelNotification, CloseSessionRequest, ContentBlock, SessionNotification, SessionUpdate,
    ToolCallContent,
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
    sync::{mpsc, oneshot},
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
/// How long the agent gets to answer the ACP handshake before the client gives
/// up. Nothing in it waits on a model, so a slow answer means a wedged agent.
const HANDSHAKE: Duration = Duration::from_secs(30);
/// Grace for the agent's last diagnostics to arrive once it has exited.
const LAST_WORDS: Duration = Duration::from_millis(250);
/// Diagnostic lines quoted back when the agent dies during the handshake.
const FAILURE_LINES: usize = 5;

pub async fn run(
    root: &Path,
    model: &str,
    a2a: &str,
    resume: Option<&str>,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // The agent fixes itself to the canonical root, so the client resolves it
    // up front: the header names a real directory and the ACP session opens on
    // the same path the agent accepts.
    let root = &root
        .canonicalize()
        .map_err(|error| Failure(format!("{}: {error}", root.display())))?;
    let persisted_session_id = resume
        .map(str::to_string)
        .unwrap_or_else(crate::session::new_id);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--model")
        .arg(model)
        .arg("--a2a")
        .arg(a2a)
        .arg("--session-id")
        .arg(&persisted_session_id);
    if resume.is_some() {
        command.arg("--resume");
    }
    if force {
        command.arg("--force");
    }
    let mut child = command
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

    // The agent's own diagnostics are the only explanation of a failed start,
    // so they are kept aside as well as shown in the log pane.
    let recent: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorder = Arc::clone(&recent);
    let diagnostics = updates_tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let update = match events::parse(&line) {
                Some(event) => Update::Runtime(event),
                None => {
                    if let Ok(mut recent) = recorder.lock() {
                        recent.push(line.clone());
                        let extra = recent.len().saturating_sub(FAILURE_LINES);
                        recent.drain(..extra);
                    }
                    Update::Log(line)
                }
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

    // The child is watched from its own task, which also owns it: aborting that
    // task drops the handle, and `kill_on_drop` takes the process with it.
    let (exit_tx, exit_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let watcher = tokio::spawn(async move {
        tokio::select! {
            status = child.wait() => {
                let _ = exit_tx.send(status);
            }
            _ = shutdown_rx => {
                // Unlike dropping a `kill_on_drop` child, `kill().await` waits
                // until the process is gone and its OS file locks are released.
                let _ = child.kill().await;
            }
        }
    });

    let notifications = updates_tx.clone();
    let cleanup_root = root.clone();
    let cleanup_session_id = persisted_session_id.clone();
    let result = agent_client_protocol::Client
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
            // An agent that dies here — a taken A2A port, a bad root, no
            // credentials — leaves its half of the handshake unanswered, and
            // waiting on it forever shows the user nothing at all. Its exit and
            // its silence both end the wait with something to read.
            let handshake = async {
                connection
                    .send_request(agentkit_acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                connection
                    .send_request(agentkit_acp::NewSessionRequest::new(root.clone()))
                    .block_task()
                    .await
            };
            let session = tokio::select! {
                session = handshake => session?,
                status = exit_rx => {
                    return Err(agent_client_protocol::Error::into_internal_error(
                        std::io::Error::other(died(status.ok().and_then(Result::ok), stderr_task, &recent).await),
                    ));
                }
                () = tokio::time::sleep(HANDSHAKE) => {
                    return Err(agent_client_protocol::Error::into_internal_error(
                        std::io::Error::other(format!(
                            "the agent did not answer the ACP handshake within {} seconds",
                            HANDSHAKE.as_secs()
                        )),
                    ));
                }
            };
            let session_id = session.session_id.clone();
            // The server has acquired the mutation lock during NewSession, so
            // this read is the exact stable snapshot it preloaded into the model.
            let restored = crate::session::load(&root, &persisted_session_id).map_err(|error| {
                agent_client_protocol::Error::into_internal_error(std::io::Error::other(error))
            })?;

            let mut terminal =
                enter().map_err(agent_client_protocol::Error::into_internal_error)?;
            let mut app = App::new(root, model, a2a);
            app.restore_transcript(persisted_session_id, &restored);
            let mut events = EventStream::new();
            let mut ticker = tokio::time::interval(TICK);
            let mut stop =
                Stop::new().map_err(agent_client_protocol::Error::into_internal_error)?;
            let result: Result<(), agent_client_protocol::Error> = async {
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
                        () = stop.requested() => return Ok(()),
                    }
                }
            }
            .await;
            leave(terminal);
            // Closing the ACP session removes its driver from the server,
            // dropping the transcript observer and its filesystem lock. Merely
            // closing stdio does not ask the headless runtime to close sessions.
            let closed = connection
                .send_request(CloseSessionRequest::new(session_id))
                .block_task()
                .await;
            result?;
            closed?;
            Ok(())
        })
        .await
        .map_err(explain);

    // The A2A listener keeps the child alive after ACP closes. Stop it only
    // after CloseSession has unwound the lock owner, then wait for OS locks to
    // be released rather than relying on `kill_on_drop`.
    let _ = shutdown_tx.send(());
    let _ = watcher.await;
    // If the server failed before acknowledging CloseSession, reclaim only a
    // lock that is now provably stale; a live owner's OS lock is never stolen.
    let _ = crate::session::remove_stale_lock(&cleanup_root, &cleanup_session_id);

    result?;
    Ok(())
}

/// An ACP error prints its whole JSON-RPC envelope; on the way out of the
/// client only the sentence inside it is worth showing.
fn explain(error: agent_client_protocol::Error) -> Box<dyn std::error::Error> {
    Box::new(Failure(match error.data {
        Some(Value::String(detail)) => detail,
        _ => error.message,
    }))
}

/// An error that reports as its own sentence, since a failed start is read by
/// a person on a terminal rather than by another program.
struct Failure(String);

impl std::fmt::Debug for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Failure {}

/// Explains an agent that exited before the session was open, quoting the last
/// thing it said.
async fn died(
    status: Option<std::process::ExitStatus>,
    stderr: tokio::task::JoinHandle<()>,
    recent: &Mutex<Vec<String>>,
) -> String {
    // Its final diagnostics are usually still in flight when it exits, and they
    // are the part worth reading.
    let _ = tokio::time::timeout(LAST_WORDS, stderr).await;
    let said = recent
        .lock()
        .map(|lines| lines.join(" · "))
        .unwrap_or_default();
    let how = match status {
        Some(status) => match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => format!("was killed ({status})"),
        },
        None => "exited".to_string(),
    };
    if said.is_empty() {
        format!("the agent {how} before the session opened")
    } else {
        format!("the agent {how} before the session opened: {said}")
    }
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
        ENHANCED.store(true, Ordering::Relaxed);
    }
    // Ratatui's own hook restores raw mode and the alternate screen, but not
    // the modes turned on above: a panic would otherwise leave the shell
    // reporting every mouse move as text.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_modes();
        previous(info);
    }));
    Ok(terminal)
}

/// Whether the keyboard enhancement flags were pushed and still need popping.
static ENHANCED: AtomicBool = AtomicBool::new(false);

/// Turns off the terminal modes the client switched on.
///
/// What was pushed is remembered rather than asked about on the way out:
/// `supports_keyboard_enhancement` queries the terminal and waits for the
/// reply, which on a torn-down or unresponsive terminal stalls the restore
/// before mouse reporting is ever turned back off.
fn restore_modes() {
    let mut stdout = std::io::stdout();
    if ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout, DisableMouseCapture, DisableBracketedPaste);
}

/// Resolves when the process is asked to stop.
///
/// A client killed from outside never reaches its restore path, which leaves
/// the shell in raw mode with mouse reporting on — every later mouse move
/// arrives at the prompt as garbage. Holding the signal streams for the whole
/// session and returning through the normal exit keeps that from happening.
struct Stop {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}

impl Stop {
    #[cfg(unix)]
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    #[cfg(not(unix))]
    fn new() -> std::io::Result<Self> {
        Ok(Self {})
    }

    #[cfg(unix)]
    async fn requested(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }

    #[cfg(not(unix))]
    async fn requested(&mut self) {
        std::future::pending().await
    }
}

fn leave(terminal: DefaultTerminal) {
    restore_modes();
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

#[cfg(test)]
mod signal_tests {
    use std::time::Duration;

    use super::Stop;

    /// A client killed from outside must still reach its restore path, or it
    /// leaves the shell in raw mode with mouse reporting on.
    #[tokio::test]
    async fn a_termination_signal_ends_the_session() {
        let mut stop = Stop::new().expect("signal handlers install");
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        std::process::Command::new("kill")
            .arg("-TERM")
            .arg(std::process::id().to_string())
            .status()
            .expect("kill runs");
        let session = async {
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    () = stop.requested() => return,
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(3), session)
            .await
            .expect("the loop leaves on the signal");
    }
}
