//! The terminal client.
//!
//! The client owns a `kit serve` child process and talks ACP to it over that
//! process's stdio, so the UI runs against exactly the protocol surface any
//! other editor would use. The child's stderr carries two things: ordinary
//! diagnostics, shown in the log pane, and the runtime side channel
//! ([`crate::events`]) that feeds the live graph of a running Runlet program.

mod app;
mod command;
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
    sync::{mpsc, oneshot},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{
    events::{self, EVENTS_ENV},
    tools::mcp::CredentialStorage,
};

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
/// Maximum time to correlate a new ACP route with its persisted transcript.
const SESSION_EVENT_WAIT: Duration = Duration::from_secs(5);
/// Grace for the agent's last diagnostics to arrive once it has exited.
const LAST_WORDS: Duration = Duration::from_millis(250);
/// Diagnostic lines quoted back when the agent dies during the handshake.
const FAILURE_LINES: usize = 5;
/// How long a turn interrupted on the way out gets to finish unwinding, so the
/// transcript it owns is closed out before the agent is killed.
const SETTLE: Duration = Duration::from_secs(3);

#[allow(clippy::too_many_arguments)]
pub async fn run(
    root: &Path,
    model: &str,
    provider: crate::ProviderKind,
    a2a: Option<&str>,
    mcp_config: Option<&Path>,
    credential_storage: &CredentialStorage,
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
    let active_persisted_id = Arc::new(Mutex::new(persisted_session_id.clone()));
    let mut command = crate::acp_child::serve_command(
        root,
        model,
        provider,
        &persisted_session_id,
        resume.is_some(),
    )?;
    if let Some(address) = a2a {
        command.arg("--a2a").arg(address);
    }
    if let Some(path) = mcp_config {
        command.arg("--mcp-config").arg(path);
    }
    command
        .arg("--mcp-credential-store")
        .arg(credential_storage.cli_name());
    if let Some(directory) = credential_storage.directory() {
        command.arg("--mcp-credential-dir").arg(directory);
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
    let a2a = a2a.unwrap_or("allocating…").to_string();
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
                None if line.starts_with("A2A listening on ") => {
                    Update::A2aAddress(line.trim_start_matches("A2A listening on ").to_string())
                }
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
    let transition_session = Arc::clone(&active_persisted_id);
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
            let mut session_id = session.session_id.clone();
            // The server has acquired the mutation lock during NewSession, so
            // this read is the exact stable snapshot it preloaded into the model.
            let restored = crate::session::load(&root, &persisted_session_id).map_err(|error| {
                agent_client_protocol::Error::into_internal_error(std::io::Error::other(error))
            })?;

            let mut terminal =
                enter().map_err(agent_client_protocol::Error::into_internal_error)?;
            let mut app = App::new(root.clone(), model, a2a);
            app.restore_transcript(persisted_session_id, &restored);
            let mut events = EventStream::new();
            let mut ticker = tokio::time::interval(TICK);
            // The turn in flight, if any: leaving is not allowed to abandon it.
            let mut turn: Option<tokio::task::JoinHandle<()>> = None;
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
                                    turn = Some(tokio::spawn(async move {
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
                                    }));
                                }
                                Action::New(first_prompt) => {
                                    // Idle-only key handling guarantees there is no active
                                    // turn to abandon. Await its completed task before closing
                                    // the old ACP driver and its transcript lock.
                                    if let Some(completed) = turn.take() {
                                        let _ = completed.await;
                                    }
                                    connection
                                        .send_request(CloseSessionRequest::new(session_id.clone()))
                                        .block_task()
                                        .await?;
                                    let session = connection
                                        .send_request(agentkit_acp::NewSessionRequest::new(root.clone()))
                                        .block_task()
                                        .await?;
                                    session_id = session.session_id;
                                    let expected_acp_session_id = session_id.to_string();
                                    // Session events share one ordered stderr stream. Apply
                                    // everything from the old session before clearing it, then
                                    // wait only for the persisted id bound to this ACP route.
                                    let persisted_id = tokio::select! {
                                        result = wait_for_started_session(
                                            &mut updates_rx,
                                            &mut app,
                                            &expected_acp_session_id,
                                            SESSION_EVENT_WAIT,
                                        ) => result.map_err(|error| {
                                            let detail = match error {
                                                SessionEventError::Closed =>
                                                    "runtime event stream closed during /new".to_string(),
                                                SessionEventError::TimedOut => format!(
                                                    "runtime did not report the new persisted session within {} seconds",
                                                    SESSION_EVENT_WAIT.as_secs(),
                                                ),
                                            };
                                            agent_client_protocol::Error::into_internal_error(
                                                std::io::Error::other(detail),
                                            )
                                        })?,
                                        () = stop.requested() => return Ok(()),
                                    };
                                    if let Ok(mut active) = transition_session.lock() {
                                        *active = persisted_id.clone();
                                    }
                                    app.start_session(persisted_id);
                                    if let Some(prompt) = first_prompt {
                                        app.push_user(prompt.clone());
                                        let connection = connection.clone();
                                        let session = session_id.clone();
                                        let updates = updates_tx.clone();
                                        turn = Some(tokio::spawn(async move {
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
                                        }));
                                    }
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
            // Quitting mid-turn — by key, by signal, or because the terminal
            // went away — must not abandon a running tool call. The agent is
            // asked to interrupt and given a moment to record the results it
            // owes, since the transcript it writes has to stay resumable even
            // though the process is about to be killed.
            if let Some(turn) = turn.filter(|turn| !turn.is_finished()) {
                let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
                let _ = tokio::time::timeout(SETTLE, turn).await;
            }
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
    if let Ok(active) = active_persisted_id.lock() {
        let _ = crate::session::remove_stale_lock(&cleanup_root, &active);
    }

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

#[derive(Debug, PartialEq, Eq)]
enum SessionEventError {
    Closed,
    TimedOut,
}

async fn wait_for_started_session(
    updates: &mut mpsc::UnboundedReceiver<Update>,
    app: &mut App,
    expected_acp_session_id: &str,
    wait: Duration,
) -> Result<String, SessionEventError> {
    tokio::time::timeout(wait, async {
        loop {
            let update = updates.recv().await.ok_or(SessionEventError::Closed)?;
            if let Some(id) = started_session(&update, expected_acp_session_id) {
                return Ok(id.to_string());
            }
            app.apply(update);
        }
    })
    .await
    .map_err(|_| SessionEventError::TimedOut)?
}

/// Returns the persisted id only when a session event belongs to the ACP
/// route just returned by `session/new`.
fn started_session<'a>(update: &'a Update, expected_acp_session_id: &str) -> Option<&'a str> {
    match update {
        Update::Runtime(events::RuntimeEvent::SessionStarted { acp_session_id, id })
            if acp_session_id == expected_acp_session_id =>
        {
            Some(id)
        }
        _ => None,
    }
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
    use std::{path::PathBuf, time::Duration};

    use super::{SessionEventError, readable, started_session, wait_for_started_session};
    use crate::{
        events::RuntimeEvent,
        tui::app::{App, Update},
    };

    #[test]
    fn correlates_persisted_sessions_with_the_new_acp_route() {
        let update = Update::Runtime(RuntimeEvent::SessionStarted {
            acp_session_id: "session-2".into(),
            id: "persisted-2".into(),
        });
        assert_eq!(started_session(&update, "session-2"), Some("persisted-2"));
        assert_eq!(started_session(&update, "session-1"), None);
    }

    #[tokio::test]
    async fn new_session_wait_is_correlated_bounded_and_detects_stream_failure() {
        let app = || App::new(PathBuf::from("/tmp"), "model".into(), "a2a".into());
        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Update::Runtime(RuntimeEvent::SessionStarted {
            acp_session_id: "other".into(),
            id: "wrong".into(),
        }))
        .unwrap();
        tx.send(Update::Runtime(RuntimeEvent::SessionStarted {
            acp_session_id: "expected".into(),
            id: "right".into(),
        }))
        .unwrap();
        assert_eq!(
            wait_for_started_session(&mut updates, &mut app(), "expected", Duration::from_secs(1),)
                .await,
            Ok("right".into())
        );

        let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        assert_eq!(
            wait_for_started_session(&mut updates, &mut app(), "expected", Duration::from_secs(1),)
                .await,
            Err(SessionEventError::Closed)
        );

        let (_tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(
            wait_for_started_session(
                &mut updates,
                &mut app(),
                "expected",
                Duration::from_millis(1),
            )
            .await,
            Err(SessionEventError::TimedOut)
        );
    }

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
