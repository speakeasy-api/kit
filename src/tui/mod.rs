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
mod image;
mod markdown;
mod plan;
mod theme;
mod ui;
mod wrap;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    ByteStreams,
    schema::{MaybeUndefined, ProtocolVersion, v2 as wire},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{mpsc, oneshot},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use wire::{
    CancelSessionNotification, CloseSessionRequest, ContentBlock, SessionConfigKind,
    SessionConfigOption, SessionConfigSelectOptions, SessionUpdate, SetSessionConfigOptionRequest,
    ToolCallContent, UpdateSessionNotification,
};

use crate::{
    events::{self, EVENTS_ENV},
    tools::mcp::CredentialStorage,
};

use app::{
    Action, App, Attachment, AttachmentKind, EffortChoice, ModelChoice, SubmittedPrompt, Update,
    UserImage,
};

/// Animation and elapsed-time refresh interval.
const TICK: Duration = Duration::from_millis(90);
/// Terminal events applied per frame, so a paste lands in one redraw.
const MAX_BURST: usize = 4_096;
const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
/// Output lines kept per tool call; the fold shows the count either way.
const MAX_OUTPUT_LINES: usize = 5_000;

#[derive(Clone)]
struct ActiveSessionRoute {
    id: String,
    generation: u64,
}

struct QueuedUpdate {
    generation: Option<u64>,
    update: Update,
}

impl QueuedUpdate {
    fn global(update: Update) -> Self {
        Self {
            generation: None,
            update,
        }
    }

    fn for_session(generation: u64, update: Update) -> Self {
        Self {
            generation: Some(generation),
            update,
        }
    }
}

fn transition_route(route: &Arc<Mutex<ActiveSessionRoute>>, id: String) {
    if let Ok(mut route) = route.lock() {
        route.id = id;
        route.generation = route.generation.wrapping_add(1);
    }
}

fn accept_queued_update(
    route: &Arc<Mutex<ActiveSessionRoute>>,
    queued: QueuedUpdate,
) -> Option<Update> {
    let accepted = queued.generation.is_none_or(|generation| {
        route
            .lock()
            .is_ok_and(|route| route.generation == generation)
    });
    accepted.then_some(queued.update)
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/background/cancel", response = CancelBackgroundResponse)]
struct CancelBackgroundRequest {
    session_id: wire::SessionId,
    call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
struct CancelBackgroundResponse {
    cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/compose/detach", response = DetachComposeResponse)]
struct DetachComposeRequest {
    session_id: wire::SessionId,
    call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
struct DetachComposeResponse {
    detached: bool,
}
/// How long the agent gets to answer the ACP handshake before the client gives
/// up. Nothing in it waits on a model, so a slow answer means a wedged agent.
const HANDSHAKE: Duration = Duration::from_secs(30);
/// Grace for an ACP session to close before the backend is terminated.
const CLOSE_SESSION: Duration = Duration::from_secs(3);
/// Grace for the agent's last diagnostics to arrive once it has exited.
const LAST_WORDS: Duration = Duration::from_millis(250);
/// Diagnostic lines quoted back when the agent dies during the handshake.
const FAILURE_LINES: usize = 5;

#[cfg(unix)]
fn detach_from_controlling_terminal(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt as _;

    // The ACP backend is headless. A new session prevents it or any nested
    // agent/tool from opening the TUI's controlling terminal via /dev/tty.
    // SAFETY: `setsid` is async-signal-safe, and this closure only reports its
    // errno if it fails.
    unsafe {
        command.as_std_mut().pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(windows)]
fn detach_from_controlling_terminal(_command: &mut tokio::process::Command) {}

fn current_model_choice(options: Option<&[SessionConfigOption]>) -> Option<ModelChoice> {
    let current = options
        .unwrap_or_default()
        .iter()
        .find(|option| option.config_id.to_string() == "model")
        .and_then(|option| match &option.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
            _ => None,
        })?;
    model_choices(options)
        .into_iter()
        .find(|choice| choice.id == current)
}

fn effort_state(options: Option<&[SessionConfigOption]>) -> Option<(String, Vec<EffortChoice>)> {
    let SessionConfigOption {
        kind: SessionConfigKind::Select(select),
        ..
    } = options?
        .iter()
        .find(|option| option.config_id.to_string() == "reasoning_effort")?
    else {
        return None;
    };
    let choices = match &select.options {
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .map(|option| EffortChoice {
                id: option.value.to_string(),
                name: option.name.clone(),
            })
            .collect(),
        _ => return None,
    };
    Some((select.current_value.to_string(), choices))
}

fn refresh_config_state(app: &mut App, options: Option<&[SessionConfigOption]>) {
    if let Some(choice) = current_model_choice(options) {
        app.provider = choice.provider;
        app.model = choice.model;
    }
    app.set_model_choices(model_choices(options));
    if let Some((current, choices)) = effort_state(options) {
        app.set_effort(current, choices);
    } else {
        app.set_effort("default".into(), Vec::new());
    }
}

fn model_choices(options: Option<&[SessionConfigOption]>) -> Vec<ModelChoice> {
    let Some(SessionConfigOption {
        kind: SessionConfigKind::Select(select),
        ..
    }) = options
        .unwrap_or_default()
        .iter()
        .find(|option| option.config_id.to_string() == "model")
    else {
        return Vec::new();
    };
    let SessionConfigSelectOptions::Grouped(groups) = &select.options else {
        return Vec::new();
    };
    groups
        .iter()
        .flat_map(|group| {
            let provider = group.group_id.to_string();
            group.options.iter().map(move |option| {
                let id = option.value.to_string();
                let model = id
                    .split_once(':')
                    .map_or_else(|| option.name.clone(), |(_, model)| model.to_string());
                ModelChoice {
                    id,
                    provider: provider.clone(),
                    model,
                }
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    root: &Path,
    model: &str,
    provider: crate::ProviderKind,
    a2a: Option<&str>,
    mcp_config: Option<&Path>,
    credential_storage: &CredentialStorage,
    telemetry: &crate::telemetry::Settings,
    resume: Option<&str>,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_reasoning_effort(
        root,
        model,
        provider,
        None,
        a2a,
        mcp_config,
        credential_storage,
        telemetry,
        resume,
        force,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_with_reasoning_effort(
    root: &Path,
    model: &str,
    provider: crate::ProviderKind,
    reasoning_effort: Option<crate::ReasoningEffort>,
    a2a: Option<&str>,
    mcp_config: Option<&Path>,
    credential_storage: &CredentialStorage,
    telemetry: &crate::telemetry::Settings,
    resume: Option<&str>,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_reasoning_effort_and_openrouter_key(
        root,
        model,
        provider,
        reasoning_effort,
        a2a,
        mcp_config,
        credential_storage,
        telemetry,
        None,
        resume,
        force,
    )
    .await
}

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn run_with_reasoning_effort_and_openrouter_key(
    root: &Path,
    model: &str,
    provider: crate::ProviderKind,
    reasoning_effort: Option<crate::ReasoningEffort>,
    a2a: Option<&str>,
    mcp_config: Option<&Path>,
    credential_storage: &CredentialStorage,
    telemetry: &crate::telemetry::Settings,
    openrouter_api_key: Option<&crate::provider::OpenRouterApiKey>,
    resume: Option<&str>,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // The agent fixes itself to the canonical root, so the client resolves it
    // up front: the header names a real directory and the ACP session opens on
    // the same path the agent accepts.
    let root = &root
        .canonicalize()
        .map_err(|error| Failure(format!("{}: {error}", root.display())))?;
    let resume_session_id = resume.map(str::to_string);
    let persisted_session_id = resume_session_id
        .clone()
        .unwrap_or_else(crate::session::new_id);
    let active_persisted_id = Arc::new(Mutex::new(ActiveSessionRoute {
        id: persisted_session_id.clone(),
        generation: 0,
    }));
    let mut command = crate::acp_child::serve_command(
        root,
        model,
        provider,
        reasoning_effort,
        openrouter_api_key,
        &persisted_session_id,
        resume_session_id.is_some(),
    )?;
    if let Some(address) = a2a {
        command.arg("--a2a").arg(address);
    }
    if let Some(path) = mcp_config {
        command.arg("--mcp-config").arg(path);
    }
    telemetry.append_cli_args(&mut command);
    command
        .arg("--credential-store")
        .arg(credential_storage.cli_name());
    if let Some(directory) = credential_storage.directory() {
        command.arg("--credential-dir").arg(directory);
    }
    if force {
        command.arg("--force");
    }
    detach_from_controlling_terminal(&mut command);
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
            if diagnostics.send(QueuedUpdate::global(update)).is_err() {
                return;
            }
        }
        // Stderr closing means the agent process is gone; stop any spinner
        // waiting on a turn that can no longer finish.
        let _ = diagnostics.send(QueuedUpdate::global(Update::ProcessExited(
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
    let notification_session = Arc::clone(&active_persisted_id);
    let result = agent_client_protocol::Client
        .v2()
        .on_receive_notification(
            async move |notification: UpdateSessionNotification, _cx| {
                let current = notification_session.lock().ok().map(|route| route.clone());
                for update in current.as_ref().map_or_else(Vec::new, |route| {
                    translate_for_session(notification, &route.id)
                }) {
                    let queued = if matches!(update, Update::AvailableCommands { .. }) {
                        QueuedUpdate::global(update)
                    } else {
                        QueuedUpdate::for_session(
                            current.as_ref().map_or(0, |route| route.generation),
                            update,
                        )
                    };
                    let _ = notifications.send(queued);
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
                let initialized = connection
                    .send_request(wire::InitializeRequest::new(
                        ProtocolVersion::V2,
                        wire::Implementation::new("kit-tui", env!("CARGO_PKG_VERSION")),
                    ))
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V2 {
                    return Err(agent_client_protocol::Error::into_internal_error(
                        std::io::Error::other("the agent did not negotiate ACP v2"),
                    ));
                }
                let can_steer = initialized.capabilities.session.as_ref()
                    .and_then(|session| session.inject.as_ref())
                    .is_some_and(|inject| {
                        inject.modes.contains(&wire::SessionInjectMode::Steer)
                            && inject.steer_in_stream.as_ref().is_some_and(|modes| {
                                modes.contains(&wire::SessionInjectSteerInStream::Finish)
                            })
                    });
                if let Some(resume_id) = resume_session_id.clone() {
                    let response = connection
                        .send_request(
                            wire::ResumeSessionRequest::new(resume_id.clone(), root.clone())
                                .replay_from(wire::ReplayFrom::Start(wire::ReplayFromStart::new())),
                        )
                        .block_task()
                        .await?;
                    Ok((wire::SessionId::new(resume_id), response.config_options, can_steer))
                } else {
                    let response = connection
                        .send_request(wire::NewSessionRequest::new(root.clone()))
                        .block_task()
                        .await?;
                    Ok((response.session_id, response.config_options, can_steer))
                }
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
            let (mut session_id, config_options, can_steer) = session;
            let active_session_id = durable_session_id(&session_id).map_err(|error| {
                agent_client_protocol::Error::into_internal_error(std::io::Error::other(error))
            })?;
            // Install every fallible signal handler before changing terminal modes so
            // an installation failure cannot leave the caller's terminal altered.
            let mut stop =
                Stop::new().map_err(agent_client_protocol::Error::into_internal_error)?;
            let (mut terminal, mut images) =
                enter().map_err(agent_client_protocol::Error::into_internal_error)?;
            let mut app = App::new(
                root.clone(),
                provider.as_str().to_string(),
                model,
                a2a,
            );
            refresh_config_state(&mut app, Some(&config_options));
            app.can_steer = can_steer;
            if let Ok(mut active) = transition_session.lock() {
                active.id = active_session_id.clone();
            }
            app.start_session(active_session_id);
            let mut events = EventStream::new();
            let mut ticker = tokio::time::interval(TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let result: Result<(), agent_client_protocol::Error> = async {
                loop {
                    terminal
                        .draw(|frame| ui::draw(frame, &mut app, &mut images))
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
                                // `EventStream::next().now_or_never()` polls with a noop
                                // waker. If no event is ready, crossterm's background reader
                                // retains that waker and cannot wake this select loop when the
                                // next key arrives. Check synchronously before polling the
                                // stream so an empty burst cannot make the TUI unresponsive.
                                if crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
                                    next = events.next().await;
                                } else {
                                    break;
                                }
                            }
                            match action {
                                Action::Quit => return Ok(()),
                                Action::Submit { prompt, inject } => {
                                    let blocks = match prompt_blocks(&prompt) {
                                        Ok(blocks) => blocks,
                                        Err(error) => {
                                            app.paste(&prompt.text);
                                            app.restore_attachments(prompt.attachments);
                                            app.note(error);
                                            continue;
                                        }
                                    };
                                    app.clear_attachments();
                                    let outcome = if inject {
                                        connection
                                            .send_request(wire::InjectSessionRequest::new(
                                                session_id.clone(),
                                                wire::SessionInjectMode::Steer,
                                                blocks,
                                            ))
                                            .block_task()
                                            .await
                                            .map(|response| Some(response.message_id))
                                    } else {
                                        connection
                                            .send_request(wire::PromptRequest::new(session_id.clone(), blocks))
                                            .block_task()
                                            .await
                                            .map(|_| None)
                                    };
                                    match outcome {
                                        Ok(Some(message_id)) => app.apply(Update::SteerAccepted {
                                            id: message_id.to_string(),
                                            text: prompt.text,
                                        }),
                                        Ok(None) => {}
                                        Err(error) => {
                                            app.paste(&prompt.text);
                                            app.restore_attachments(prompt.attachments);
                                            app.note(format!("message was not accepted: {}", error.message));
                                        }
                                    }
                                }
                                Action::New(first_prompt) => {
                                    connection
                                        .send_request(CloseSessionRequest::new(session_id.clone()))
                                        .block_task()
                                        .await?;
                                    let session = connection
                                        .send_request(wire::NewSessionRequest::new(root.clone()))
                                        .block_task()
                                        .await?;
                                    session_id = session.session_id;
                                    let persisted_id = durable_session_id(&session_id).map_err(|error| {
                                        agent_client_protocol::Error::into_internal_error(std::io::Error::other(error))
                                    })?;
                                    transition_route(&transition_session, persisted_id.clone());
                                    images.clear();
                                    app.start_session(persisted_id);
                                    refresh_config_state(&mut app, Some(&session.config_options));
                                    if let Some(prompt) = first_prompt {
                                        let outcome = connection
                                            .send_request(wire::PromptRequest::new(
                                                session_id.clone(),
                                                vec![ContentBlock::Text(wire::TextContent::new(prompt))],
                                            ))
                                            .block_task()
                                            .await;
                                        if let Err(error) = outcome {
                                            app.note(format!("message was not accepted: {}", error.message));
                                        }
                                    }
                                }
                                Action::ListSessions => {
                                    let Ok(route) = transition_session.lock() else {
                                        app.note("could not start session catalog scan");
                                        continue;
                                    };
                                    let generation = route.generation;
                                    drop(route);
                                    let root = root.clone();
                                    let updates = updates_tx.clone();
                                    tokio::spawn(async move {
                                        let result = tokio::task::spawn_blocking(move || {
                                            crate::session::catalog(&root)
                                        })
                                        .await
                                        .map_err(|error| {
                                            format!("session catalog worker failed: {error}")
                                        })
                                        .and_then(|result| result);
                                        let _ = updates.send(QueuedUpdate::for_session(
                                            generation,
                                            Update::SessionCatalog(result),
                                        ));
                                    });
                                }
                                Action::RenameSession {
                                    session_id: requested_id,
                                    display_name,
                                } => {
                                    let Ok(route) = transition_session.lock() else {
                                        app.note("could not start session rename");
                                        continue;
                                    };
                                    let generation = route.generation;
                                    drop(route);
                                    let root = root.clone();
                                    let updates = updates_tx.clone();
                                    tokio::spawn(async move {
                                        let name_for_write = display_name.clone();
                                        let id_for_write = requested_id.clone();
                                        let result = tokio::task::spawn_blocking(move || {
                                            crate::session::set_display_name_and_title(
                                                &root,
                                                &id_for_write,
                                                name_for_write.as_deref(),
                                            )
                                        })
                                        .await
                                        .map_err(|error| {
                                            format!("session rename worker failed: {error}")
                                        })
                                        .and_then(|result| result);
                                        let display_name = display_name
                                            .map(|name| name.trim().to_string());
                                        let _ = updates.send(QueuedUpdate::for_session(
                                            generation,
                                            Update::SessionRenamed {
                                                session_id: requested_id,
                                                display_name,
                                                result,
                                            },
                                        ));
                                    });
                                }
                                Action::Resume(requested_id) => {
                                    if let Err(error) = crate::session::validate_id(&requested_id) {
                                        app.note(format!("invalid session id: {error}"));
                                        continue;
                                    }
                                    let Some(previous_persisted_id) =
                                        previous_session_for_resume(&session_id, &requested_id)
                                            .map_err(|error| {
                                                agent_client_protocol::Error::into_internal_error(
                                                    std::io::Error::other(error),
                                                )
                                            })?
                                    else {
                                        app.note(format!("session {requested_id} is already active"));
                                        continue;
                                    };
                                    // Session switching has no `--force` flag. Reclaim only a
                                    // lock that the OS proves no live Kit process still holds.
                                    if let Err(error) = crate::session::load(&root, &requested_id)
                                        .and_then(|_| {
                                            crate::session::remove_stale_lock(&root, &requested_id)
                                        })
                                    {
                                        app.note(format!("could not resume session: {error}"));
                                        continue;
                                    }
                                    let previous_session_id = session_id.clone();
                                    if let Err(error) = connection
                                        .send_request(CloseSessionRequest::new(session_id.clone()))
                                        .block_task()
                                        .await
                                    {
                                        app.note(format!(
                                            "could not close the current session: {}",
                                            error.message
                                        ));
                                        continue;
                                    }
                                    session_id = wire::SessionId::new(requested_id.clone());
                                    transition_route(&transition_session, requested_id.clone());
                                    images.clear();
                                    app.start_session(requested_id.clone());
                                    match request_resume(&connection, session_id.clone(), root.clone()).await {
                                        Ok(response) => refresh_config_state(
                                            &mut app,
                                            Some(&response.config_options),
                                        ),
                                        Err(error) => {
                                            session_id = previous_session_id;
                                            transition_route(
                                                &transition_session,
                                                previous_persisted_id.clone(),
                                            );
                                            images.clear();
                                            app.start_session(previous_persisted_id);
                                            let restored = request_resume(
                                                &connection,
                                                session_id.clone(),
                                                root.clone(),
                                            )
                                            .await;
                                            match restored {
                                                Ok(response) => {
                                                    refresh_config_state(
                                                        &mut app,
                                                        Some(&response.config_options),
                                                    );
                                                    app.note(format!(
                                                        "could not resume {requested_id}: {}",
                                                        error.message
                                                    ));
                                                }
                                                Err(restore_error) => {
                                                    return Err(agent_client_protocol::Error::into_internal_error(
                                                        std::io::Error::other(format!(
                                                            "could not resume {requested_id}: {}; could not restore the previous session: {}",
                                                            error.message, restore_error.message
                                                        )),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                                Action::Close => return Ok(()),
                                Action::SelectModel { choice, save_defaults } => {
                                    let response = connection.send_request(
                                        SetSessionConfigOptionRequest::new(
                                            session_id.clone(), "model", choice.id.as_str(),
                                        ),
                                    ).block_task().await;
                                    match response {
                                        Ok(response) => {
                                            app.usage = None;
                                            refresh_config_state(
                                                &mut app,
                                                Some(&response.config_options),
                                            );
                                            app.note(format!("model changed to {} via {}", choice.model, choice.provider));
                                            if save_defaults {
                                                match save_model_defaults(&choice) {
                                                    Ok(()) => app.note("saved model defaults to ~/.kit/config.toml"),
                                                    Err(error) => app.note(format!("model changed, but defaults were not saved: {error}")),
                                                }
                                            }
                                        }
                                        Err(error) => app.note(format!("model change failed: {}", error.message)),
                                    }
                                }
                                Action::SelectEffort { effort, save_defaults } => {
                                    let response = connection
                                        .send_request(SetSessionConfigOptionRequest::new(
                                            session_id.clone(),
                                            "reasoning_effort",
                                            effort.as_str(),
                                        ))
                                        .block_task()
                                        .await;
                                    match response {
                                        Ok(response) => {
                                            refresh_config_state(
                                                &mut app,
                                                Some(&response.config_options),
                                            );
                                            app.note(format!(
                                                "reasoning effort changed to {effort}"
                                            ));
                                            if save_defaults {
                                                match save_effort_default(&effort) {
                                                    Ok(()) => app.note(
                                                        "saved reasoning effort default to ~/.kit/config.toml",
                                                    ),
                                                    Err(error) => app.note(format!(
                                                        "reasoning effort changed, but default was not saved: {error}"
                                                    )),
                                                }
                                            }
                                        }
                                        Err(error) => app.note(format!(
                                            "reasoning effort change failed: {}",
                                            error.message
                                        )),
                                    }
                                }
                                Action::Copy(text) => {
                                    execute!(terminal.backend_mut(), Print(osc52(&text)))
                                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                                }
                                Action::Cancel => {
                                    let _ = connection.send_notification(
                                        CancelSessionNotification::new(session_id.clone()),
                                    );
                                }
                                Action::DetachCompose(call_id) => {
                                    match connection
                                        .send_request(DetachComposeRequest {
                                            session_id: session_id.clone(),
                                            call_id,
                                        })
                                        .block_task()
                                        .await
                                    {
                                        Ok(response) if !response.detached => {
                                            app.note("compose call is no longer running in the foreground");
                                        }
                                        Err(error) => app.note(format!(
                                            "could not background compose call: {}", error.message
                                        )),
                                        Ok(_) => {}
                                    }
                                }
                                Action::CancelBackground(call_id) => {
                                    let response = connection
                                        .send_request(CancelBackgroundRequest {
                                            session_id: session_id.clone(),
                                            call_id: call_id.clone(),
                                        })
                                        .block_task()
                                        .await;
                                    if let Err(error) = response {
                                        app.note(format!(
                                            "could not cancel background call: {}", error.message
                                        ));
                                    }
                                }
                                Action::None | Action::Redraw => {}
                            }
                        },
                        update = updates_rx.recv() => match update {
                            Some(update) => {
                                if let Some(update) = accept_queued_update(&transition_session, update) {
                                    if let Update::ConfigOptions(options) = &update {
                                        refresh_config_state(&mut app, Some(options));
                                    }
                                    app.apply(update);
                                }
                                while let Ok(update) = updates_rx.try_recv() {
                                    let Some(update) = accept_queued_update(&transition_session, update) else {
                                        continue;
                                    };
                                    if let Update::ConfigOptions(options) = &update {
                                        refresh_config_state(&mut app, Some(options));
                                    }
                                    app.apply(update);
                                }
                            }
                            None => return Ok(()),
                        },
                        _ = ticker.tick(), if app.needs_redraw_tick() => app.tick(),
                        () = stop.requested() => return Ok(()),
                    }
                }
            }
            .await;
            leave(terminal);
            // Closing the ACP session removes its driver from the server,
            // dropping the transcript observer and its filesystem lock. Merely
            // closing stdio does not ask the headless runtime to close sessions.
            let closed = bounded_graceful_close(
                connection
                    .send_request(CloseSessionRequest::new(session_id))
                    .block_task(),
                stop.requested(),
                CLOSE_SESSION,
            )
            .await;
            result?;
            if let Some(closed) = closed {
                closed?;
            }
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
        let _ = crate::session::remove_stale_lock(&cleanup_root, &active.id);
    }

    result?;
    Ok(())
}

fn save_effort_default(effort: &str) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "HOME is unset; cannot save defaults".to_string())?;
    save_effort_default_to(
        &std::path::PathBuf::from(home).join(".kit/config.toml"),
        effort,
    )
}

fn save_effort_default_to(path: &Path, effort: &str) -> Result<(), String> {
    update_config(path, |root| {
        if effort == "default" {
            root.remove("reasoning_effort");
        } else {
            root.insert(
                "reasoning_effort".into(),
                toml::Value::String(effort.to_string()),
            );
        }
    })
}

fn save_model_defaults(choice: &ModelChoice) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "HOME is unset; cannot save defaults".to_string())?;
    save_model_defaults_to(
        &std::path::PathBuf::from(home).join(".kit/config.toml"),
        choice,
    )
}

fn save_model_defaults_to(path: &Path, choice: &ModelChoice) -> Result<(), String> {
    update_config(path, |root| {
        root.insert(
            "provider".into(),
            toml::Value::String(choice.provider.clone()),
        );
        root.insert("model".into(), toml::Value::String(choice.model.clone()));
    })
}

fn update_config(
    path: &Path,
    update: impl FnOnce(&mut toml::map::Map<String, toml::Value>),
) -> Result<(), String> {
    use std::io::Write as _;

    let contents = match std::fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let mut config = if contents.is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str::<toml::Value>(&contents)
            .map_err(|error| format!("invalid {}: {error}", path.display()))?
    };
    let root = config
        .as_table_mut()
        .ok_or_else(|| format!("invalid {}: root must be a table", path.display()))?;
    update(root);
    let output = toml::to_string_pretty(&config)
        .map_err(|error| format!("could not serialize {}: {error}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| "config path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write(|file| file.write_all(output.as_bytes()))
        .map_err(|error| format!("could not save {}: {error}", path.display()))
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

/// Encodes exact source text for the terminal clipboard.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}

/// Applies one terminal event, returning the work it asks for.
fn handle(app: &mut App, event: Event) -> Action {
    match event {
        Event::Key(key) => app.handle_key(key),
        Event::Mouse(mouse) => app.handle_mouse(mouse),
        Event::Paste(text) => {
            if app.session_rename_active() {
                app.paste(&text);
            } else if let Some(attachments) = attachments_from_paste(&app.root, &text) {
                app.prune_attachments();
                let pending_bytes = app
                    .attachments
                    .iter()
                    .chain(&attachments)
                    .try_fold(0_u64, |total, attachment| {
                        total.checked_add(attachment.size)
                    });
                if app.attachments.len() + attachments.len() > MAX_ATTACHMENTS {
                    app.note(format!(
                        "at most {MAX_ATTACHMENTS} attachments can be pending"
                    ));
                } else if pending_bytes.is_none_or(|total| total > MAX_TOTAL_ATTACHMENT_BYTES) {
                    app.note("attachments exceed the 20 MiB total limit");
                } else {
                    for attachment in attachments {
                        app.attach(
                            attachment.path,
                            attachment.mime_type,
                            attachment.kind,
                            attachment.size,
                        );
                    }
                }
            } else {
                app.paste(&text);
            }
            Action::None
        }
        _ => Action::None,
    }
}

fn attachments_from_paste(root: &Path, text: &str) -> Option<Vec<Attachment>> {
    let direct = text.trim();
    let candidates = if media_attachment(root, direct).is_some() {
        vec![direct.to_string()]
    } else {
        shlex::split(direct)?
    };
    if candidates.is_empty() || candidates.len() > MAX_ATTACHMENTS {
        return None;
    }
    candidates
        .into_iter()
        .map(|candidate| media_attachment(root, &candidate))
        .collect()
}

fn media_attachment(root: &Path, value: &str) -> Option<Attachment> {
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let path = path.canonicalize().ok()?;
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES {
        return None;
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let (kind, mime_type) = match extension.as_str() {
        "png" => (AttachmentKind::Image, "image/png"),
        "jpg" | "jpeg" => (AttachmentKind::Image, "image/jpeg"),
        "gif" => (AttachmentKind::Image, "image/gif"),
        "webp" => (AttachmentKind::Image, "image/webp"),
        "wav" => (AttachmentKind::Audio, "audio/wav"),
        "mp3" => (AttachmentKind::Audio, "audio/mpeg"),
        _ => return None,
    };
    Some(Attachment {
        path,
        placeholder: String::new(),
        mime_type,
        kind,
        size: metadata.len(),
    })
}

fn prompt_blocks(prompt: &SubmittedPrompt) -> Result<Vec<ContentBlock>, String> {
    let mut total = 0_u64;
    let mut media = Vec::with_capacity(prompt.attachments.len());
    for attachment in &prompt.attachments {
        let bytes = std::fs::read(&attachment.path)
            .map_err(|error| format!("could not read {}: {error}", attachment.path.display()))?;
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "{} exceeds the 10 MiB limit",
                attachment.path.display()
            ));
        }
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "attachment size overflow".to_string())?;
        if total > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err("attachments exceed the 20 MiB total limit".into());
        }
        media.push((attachment, bytes));
    }

    let mut model_text = prompt.text.clone();
    for attachment in &prompt.attachments {
        if !attachment.placeholder.is_empty()
            && let Ok(uri) = url::Url::from_file_path(&attachment.path)
        {
            model_text = model_text.replace(
                &attachment.placeholder,
                &format!(
                    "[{}]({uri})",
                    attachment.placeholder.trim_matches(['[', ']'])
                ),
            );
        }
    }
    let mut blocks = vec![ContentBlock::Text(wire::TextContent::new(model_text))];
    for (attachment, bytes) in media {
        let data = STANDARD.encode(bytes);
        blocks.push(match attachment.kind {
            AttachmentKind::Image => {
                let uri = url::Url::from_file_path(&attachment.path)
                    .ok()
                    .map(|uri| uri.to_string());
                ContentBlock::Image(wire::ImageContent::new(data, attachment.mime_type).uri(uri))
            }
            AttachmentKind::Audio => {
                ContentBlock::Audio(wire::AudioContent::new(data, attachment.mime_type))
            }
        });
    }
    Ok(blocks)
}

fn enter() -> std::io::Result<(DefaultTerminal, image::ImageRuntime)> {
    let terminal = ratatui::try_init()?;
    // Query after entering the alternate screen but before the event stream owns
    // terminal input, as required by ratatui-image. The query has a short bound.
    let images = image::ImageRuntime::detect();
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
    Ok((terminal, images))
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
    interrupt: tokio::signal::unix::Signal,
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
            interrupt: signal(SignalKind::interrupt())?,
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
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }

    #[cfg(not(unix))]
    async fn requested(&mut self) {
        std::future::pending().await
    }
}

async fn bounded_graceful_close<T>(
    close: impl std::future::Future<Output = T>,
    stop: impl std::future::Future<Output = ()>,
    grace: Duration,
) -> Option<T> {
    tokio::select! {
        output = close => Some(output),
        () = stop => None,
        () = tokio::time::sleep(grace) => None,
    }
}

fn leave(terminal: DefaultTerminal) {
    restore_modes();
    drop(terminal);
    ratatui::restore();
}

async fn request_resume(
    connection: &agent_client_protocol::V2ConnectionTo<agent_client_protocol::Agent>,
    session_id: wire::SessionId,
    root: PathBuf,
) -> Result<wire::ResumeSessionResponse, agent_client_protocol::Error> {
    connection
        .send_request(
            wire::ResumeSessionRequest::new(session_id, root)
                .replay_from(wire::ReplayFrom::Start(wire::ReplayFromStart::new())),
        )
        .block_task()
        .await
}

fn durable_session_id(session_id: &wire::SessionId) -> Result<String, String> {
    let session_id = session_id.to_string();
    crate::session::validate_id(&session_id)?;
    Ok(session_id)
}

fn previous_session_for_resume(
    current: &wire::SessionId,
    requested: &str,
) -> Result<Option<String>, String> {
    let current = durable_session_id(current)?;
    Ok((current != requested).then_some(current))
}

/// Maps one ACP session notification onto client updates.
fn translate(notification: UpdateSessionNotification) -> (String, Vec<Update>) {
    let session_id = notification.session_id.to_string();
    let updates = match notification.update {
        SessionUpdate::UserMessageChunk(chunk) => {
            let (text, images) = user_message_of(vec![chunk.content]);
            vec![Update::UserMessage {
                id: chunk.message_id.to_string(),
                text,
                images,
                append: true,
            }]
        }
        SessionUpdate::AgentMessageChunk(chunk) => message_of(chunk.content)
            .map(|text| Update::AgentMessage {
                id: chunk.message_id.to_string(),
                text,
                append: true,
            })
            .into_iter()
            .collect(),
        SessionUpdate::AgentThoughtChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) => vec![Update::AgentThought {
                id: chunk.message_id.to_string(),
                text: text.text,
                append: true,
            }],
            _ => Vec::new(),
        },
        SessionUpdate::UserMessage(message) => message_patch(
            message.message_id.to_string(),
            message.content,
            MessageKind::User,
        ),
        SessionUpdate::AgentMessage(message) => message_patch(
            message.message_id.to_string(),
            message.content,
            MessageKind::Agent,
        ),
        SessionUpdate::AgentThought(message) => message_patch(
            message.message_id.to_string(),
            message.content,
            MessageKind::Thought,
        ),
        SessionUpdate::ToolCallUpdate(update) => {
            let output = match &update.content {
                MaybeUndefined::Value(content) => Some(output_of(Some(content))),
                MaybeUndefined::Null => Some(Vec::new()),
                MaybeUndefined::Undefined => match &update.raw_output {
                    MaybeUndefined::Value(output) => Some(raw_output_lines(output)),
                    MaybeUndefined::Null => Some(Vec::new()),
                    MaybeUndefined::Undefined => None,
                },
            };
            let script = match &update.raw_input {
                MaybeUndefined::Value(input) => Some(script_of(input).unwrap_or_default()),
                MaybeUndefined::Null => Some(String::new()),
                MaybeUndefined::Undefined => None,
            };
            let backgrounded = update
                .raw_input
                .value()
                .and_then(|input| input.get("background"))
                .and_then(Value::as_bool)
                == Some(true)
                || output.as_ref().is_some_and(|lines| {
                    lines
                        .iter()
                        .any(|line| line.contains("is now running in the background"))
                });
            vec![Update::ToolPatched {
                id: update.tool_call_id.to_string(),
                title: match update.title {
                    MaybeUndefined::Value(title) => Some(title),
                    MaybeUndefined::Null => Some("Tool".into()),
                    MaybeUndefined::Undefined => None,
                },
                kind: match update.kind {
                    MaybeUndefined::Value(kind) => Some(kind),
                    MaybeUndefined::Null => Some(wire::ToolKind::default()),
                    MaybeUndefined::Undefined => None,
                },
                status: match update.status {
                    MaybeUndefined::Value(status) => Some(status),
                    MaybeUndefined::Null => Some(wire::ToolCallStatus::default()),
                    MaybeUndefined::Undefined => None,
                },
                script,
                output,
                append_output: false,
                backgrounded,
            }]
        }
        SessionUpdate::ToolCallContentChunk(chunk) => {
            let output = output_of(Some(std::slice::from_ref(&chunk.content)));
            let backgrounded = output
                .iter()
                .any(|line| line.contains("is now running in the background"));
            vec![Update::ToolPatched {
                id: chunk.tool_call_id.to_string(),
                title: None,
                kind: None,
                status: None,
                script: None,
                output: Some(output),
                append_output: true,
                backgrounded,
            }]
        }
        SessionUpdate::AvailableCommandsUpdate(update) => vec![Update::AvailableCommands {
            session_id: session_id.clone(),
            commands: update
                .available_commands
                .into_iter()
                .map(|available| command::Command::new(available.name, available.description))
                .collect(),
        }],
        SessionUpdate::ConfigOptionUpdate(update) => {
            vec![Update::ConfigOptions(update.config_options)]
        }
        SessionUpdate::UsageUpdate(usage) => vec![Update::Usage {
            used: usage.used,
            size: usage.size,
        }],
        SessionUpdate::StateUpdate(state) => match state {
            wire::StateUpdate::Running(_) => vec![Update::State {
                active: true,
                steerable: true,
                cancelled: false,
            }],
            wire::StateUpdate::RequiresAction(_) => vec![Update::State {
                active: true,
                steerable: false,
                cancelled: false,
            }],
            wire::StateUpdate::Idle(idle) => vec![Update::Stopped(idle.stop_reason)],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    (session_id, updates)
}

fn translate_for_session(notification: UpdateSessionNotification, current: &str) -> Vec<Update> {
    let (session_id, updates) = translate(notification);
    updates
        .into_iter()
        .filter(|update| {
            session_id == current || matches!(update, Update::AvailableCommands { .. })
        })
        .collect()
}

#[derive(Clone, Copy)]
enum MessageKind {
    User,
    Agent,
    Thought,
}

fn message_patch(
    id: String,
    content: MaybeUndefined<Vec<ContentBlock>>,
    kind: MessageKind,
) -> Vec<Update> {
    let blocks = match content {
        MaybeUndefined::Undefined => return Vec::new(),
        MaybeUndefined::Null => Vec::new(),
        MaybeUndefined::Value(blocks) => blocks,
    };
    if matches!(kind, MessageKind::User) {
        let (text, images) = user_message_of(blocks);
        return vec![Update::UserMessage {
            id,
            text,
            images,
            append: false,
        }];
    }
    let text = blocks
        .into_iter()
        .filter_map(message_of)
        .collect::<Vec<_>>()
        .join("");
    vec![match kind {
        MessageKind::User => unreachable!("handled above"),
        MessageKind::Agent => Update::AgentMessage {
            id,
            text,
            append: false,
        },
        MessageKind::Thought => Update::AgentThought {
            id,
            text,
            append: false,
        },
    }]
}

fn user_message_of(blocks: Vec<ContentBlock>) -> (String, Vec<UserImage>) {
    let mut text = String::new();
    let mut images = Vec::new();
    let mut image_ordinal = 0;
    let mut separate_after_image = false;

    for block in blocks {
        match block {
            ContentBlock::Image(image) => {
                image_ordinal += 1;
                let uri = image.uri.filter(|uri| safe_media_uri(uri));
                let existing_line = uri
                    .as_deref()
                    .and_then(|uri| markdown::line_with_link(&text, uri));
                let line =
                    existing_line.unwrap_or_else(|| {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        let line = text.bytes().filter(|&byte| byte == b'\n').count();
                        let label = format!("Image #{image_ordinal}");
                        text.push_str(&uri.as_ref().map_or_else(
                            || format!("[{label}]"),
                            |uri| format!("[{label}]({uri})"),
                        ));
                        separate_after_image = true;
                        line
                    });
                if let Some(image) = UserImage::new(image.data, image.mime_type.to_string(), line) {
                    images.push(image);
                }
            }
            block => {
                let Some(content) = message_of(block) else {
                    continue;
                };
                if separate_after_image
                    && !text.ends_with('\n')
                    && !content.starts_with('\n')
                    && !content.is_empty()
                {
                    text.push('\n');
                }
                text.push_str(&content);
                separate_after_image = false;
            }
        }
    }
    (text, images)
}

fn message_of(content: ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text),
        ContentBlock::Image(image) => Some(
            image
                .uri
                .filter(|uri| safe_media_uri(uri))
                .map_or_else(|| "[Image]".to_string(), |uri| format!("[Image]({uri})")),
        ),
        ContentBlock::Audio(_) => Some("[Audio]".into()),
        ContentBlock::ResourceLink(link) => {
            safe_media_uri(link.uri.as_ref()).then(|| format!("[{}]({})", link.name, link.uri))
        }
        ContentBlock::Resource(_) => Some("[Media resource]".into()),
        _ => None,
    }
}

fn safe_media_uri(uri: &str) -> bool {
    uri.len() <= 2_048
        && url::Url::parse(uri).is_ok_and(|uri| matches!(uri.scheme(), "file" | "http" | "https"))
}

/// The Runlet program inside a `compose` call's input, when there is one.
fn script_of(input: &Value) -> Option<String> {
    input
        .get("script")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A tool call's output as readable lines, kept whole for the folded card.
fn raw_output_lines(output: &Value) -> Vec<String> {
    if let Some(text) = output
        .as_str()
        .or_else(|| output.get("text").and_then(Value::as_str))
    {
        return readable(text);
    }
    serde_json::to_string_pretty(output)
        .unwrap_or_else(|_| output.to_string())
        .lines()
        .map(str::to_string)
        .collect()
}

fn output_of(content: Option<&[ToolCallContent]>) -> Vec<String> {
    let mut output = Vec::new();
    for entry in content.unwrap_or_default() {
        match entry {
            ToolCallContent::Content(content) => {
                if let Some(text) = message_of(content.content.clone()) {
                    output.extend(readable(&text));
                }
            }
            ToolCallContent::Diff(diff) => output.push(format!(
                "{} files · {} lines",
                diff.changes.len(),
                diff.patch
                    .as_ref()
                    .map_or(0, |patch| patch.text.lines().count())
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
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use agent_client_protocol::schema::v2::{
        AgentMessage, AgentThought, AvailableCommand, AvailableCommandsUpdate, ContentBlock,
        IdleStateUpdate, RunningStateUpdate, SessionConfigOption, SessionConfigSelectGroup,
        SessionConfigSelectOption, SessionUpdate, StateUpdate, TextContent,
        UpdateSessionNotification, UserMessage,
    };
    use crossterm::event::Event;

    use serde_json::json;

    use super::{
        ActiveSessionRoute, MAX_ATTACHMENTS, ModelChoice, QueuedUpdate, accept_queued_update,
        attachments_from_paste, command, current_model_choice, detach_from_controlling_terminal,
        durable_session_id, effort_state, handle, message_of, osc52, previous_session_for_resume,
        prompt_blocks, readable, refresh_config_state, save_effort_default_to,
        save_model_defaults_to, transition_route, translate, translate_for_session,
        user_message_of, wire,
    };
    use crate::tui::app::{App, SessionDialog, SessionRename, SubmittedPrompt, Update};

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_backend_starts_in_a_detached_session() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("sleep 30");
        detach_from_controlling_terminal(&mut command);

        let mut child = command.spawn().unwrap();
        let pid = i32::try_from(child.id().unwrap()).unwrap();
        // SAFETY: `pid` belongs to the live child above.
        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[test]
    fn resuming_the_active_session_is_a_noop() {
        let current = wire::SessionId::new("current");
        assert_eq!(
            previous_session_for_resume(&current, "current").unwrap(),
            None
        );
        assert_eq!(
            previous_session_for_resume(&current, "other")
                .unwrap()
                .as_deref(),
            Some("current")
        );
    }

    #[test]
    fn queued_updates_from_previous_session_generations_are_dropped() {
        let route = Arc::new(Mutex::new(ActiveSessionRoute {
            id: "first".into(),
            generation: 0,
        }));
        let old = QueuedUpdate::for_session(0, Update::Log("old".into()));

        transition_route(&route, "second".into());
        assert!(accept_queued_update(&route, old).is_none());
        transition_route(&route, "first".into());
        assert!(
            accept_queued_update(
                &route,
                QueuedUpdate::for_session(0, Update::Log("older attachment".into())),
            )
            .is_none()
        );
        assert!(
            accept_queued_update(
                &route,
                QueuedUpdate::for_session(2, Update::Log("current".into())),
            )
            .is_some()
        );
        assert!(
            accept_queued_update(&route, QueuedUpdate::global(Update::Log("global".into())))
                .is_some()
        );
    }

    #[test]
    fn translates_available_commands_with_their_session() {
        let (_, updates) = translate(UpdateSessionNotification::new(
            "session",
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("compact", "Compact context"),
            ])),
        ));

        assert!(matches!(
            updates.as_slice(),
            [Update::AvailableCommands { session_id, commands }]
                if session_id == "session"
                    && commands.as_slice()
                        == [command::Command::new("compact", "Compact context")]
        ));
    }

    #[test]
    fn translates_accepted_user_messages_and_foreground_state() {
        let user = UpdateSessionNotification::new(
            "session",
            SessionUpdate::UserMessage(
                UserMessage::new("user-1")
                    .content(vec![ContentBlock::Text(TextContent::new("steer"))]),
            ),
        );
        assert!(matches!(
            translate_for_session(user, "session").as_slice(),
            [Update::UserMessage { id, text, append: false, .. }]
                if id == "user-1" && text == "steer"
        ));

        let running = UpdateSessionNotification::new(
            "session",
            SessionUpdate::StateUpdate(StateUpdate::Running(RunningStateUpdate::new())),
        );
        assert!(matches!(
            translate_for_session(running, "session").as_slice(),
            [Update::State {
                active: true,
                steerable: true,
                cancelled: false
            }]
        ));
        let idle = UpdateSessionNotification::new(
            "session",
            SessionUpdate::StateUpdate(StateUpdate::Idle(IdleStateUpdate::new())),
        );
        assert!(matches!(
            translate_for_session(idle, "session").as_slice(),
            [Update::Stopped(None)]
        ));
    }

    #[test]
    fn translates_empty_agent_replacements_as_whole_message_patches() {
        let message = UpdateSessionNotification::new(
            "session",
            SessionUpdate::AgentMessage(AgentMessage::new("message").content(Vec::new())),
        );
        assert!(matches!(
            translate_for_session(message, "session").as_slice(),
            [Update::AgentMessage { id, text, append: false }]
                if id == "message" && text.is_empty()
        ));

        let thought = UpdateSessionNotification::new(
            "session",
            SessionUpdate::AgentThought(AgentThought::new("thought").content(Vec::new())),
        );
        assert!(matches!(
            translate_for_session(thought, "session").as_slice(),
            [Update::AgentThought { id, text, append: false }]
                if id == "thought" && text.is_empty()
        ));
    }

    #[test]
    fn translates_replayed_raw_tool_output() {
        let update = UpdateSessionNotification::new(
            "session",
            SessionUpdate::ToolCallUpdate(
                wire::ToolCallUpdate::new("tool-1")
                    .status(wire::ToolCallStatus::Completed)
                    .raw_output(Some(json!({ "text": "first\nsecond" }))),
            ),
        );

        assert!(matches!(
            translate_for_session(update, "session").as_slice(),
            [Update::ToolPatched { output: Some(output), .. }]
                if output == &["first", "second"]
        ));
    }

    #[test]
    fn defers_session_scoped_commands_but_drops_other_inactive_updates() {
        let commands = UpdateSessionNotification::new(
            "next",
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![])),
        );
        assert!(matches!(
            translate_for_session(commands, "current").as_slice(),
            [Update::AvailableCommands { session_id, .. }] if session_id == "next"
        ));

        let state = UpdateSessionNotification::new(
            "next",
            SessionUpdate::StateUpdate(wire::StateUpdate::Running(wire::RunningStateUpdate::new())),
        );
        assert!(translate_for_session(state, "current").is_empty());
    }

    #[test]
    fn dropped_shell_escaped_image_path_becomes_an_attachment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Screenshot 2026.png");
        std::fs::write(&path, b"png").unwrap();
        let pasted = path.display().to_string().replace(' ', "\\ ");

        let attachments = attachments_from_paste(directory.path(), &pasted).unwrap();

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, path.canonicalize().unwrap());
        assert_eq!(attachments[0].mime_type, "image/png");
    }

    #[test]
    fn multiple_dropped_paths_become_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first image.png");
        let second = directory.path().join("second.mp3");
        std::fs::write(&first, b"png").unwrap();
        std::fs::write(&second, b"mp3").unwrap();
        let pasted = format!("\"{}\" \"{}\"", first.display(), second.display());

        let attachments = attachments_from_paste(directory.path(), &pasted).unwrap();

        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].mime_type, "image/png");
        assert_eq!(attachments[1].mime_type, "audio/mpeg");
    }

    #[test]
    fn mixed_text_and_path_remains_ordinary_paste() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"png").unwrap();

        assert!(
            attachments_from_paste(directory.path(), &format!("describe {}", path.display()))
                .is_none()
        );
    }

    #[test]
    fn pending_attachment_limit_applies_across_pastes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"png").unwrap();
        let mut app = App::new(
            directory.path().into(),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );

        for _ in 0..MAX_ATTACHMENTS {
            handle(&mut app, Event::Paste(path.display().to_string()));
        }
        handle(&mut app, Event::Paste(path.display().to_string()));

        assert_eq!(app.attachments.len(), MAX_ATTACHMENTS);
    }

    #[test]
    fn session_rename_paste_is_not_interpreted_as_an_attachment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"png").unwrap();
        let mut app = App::new(
            directory.path().into(),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.session_dialog = Some(SessionDialog {
            selected: 0,
            rename: Some(SessionRename::Editing(String::new())),
        });

        handle(&mut app, Event::Paste(path.display().to_string()));

        assert!(app.attachments.is_empty());
        assert!(matches!(
            app.session_dialog.as_ref().unwrap().rename.as_ref(),
            Some(SessionRename::Editing(input)) if input == path.to_str().unwrap()
        ));
    }

    #[test]
    fn session_rename_confirmation_and_saving_consume_paste() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"png").unwrap();

        for rename in [SessionRename::ConfirmClear, SessionRename::Saving] {
            let mut app = App::new(
                directory.path().into(),
                "provider".into(),
                "model".into(),
                "a2a".into(),
            );
            app.session_dialog = Some(SessionDialog {
                selected: 0,
                rename: Some(rename),
            });

            handle(&mut app, Event::Paste(path.display().to_string()));

            assert!(app.attachments.is_empty());
            assert!(app.editor.is_empty());
            assert!(app.session_rename_active());
        }
    }

    #[test]
    fn image_attachment_is_resolved_into_an_acp_prompt_block() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"png").unwrap();
        let mut attachment = attachments_from_paste(directory.path(), path.to_str().unwrap())
            .unwrap()
            .remove(0);
        attachment.placeholder = "[Image #1]".into();
        let prompt = SubmittedPrompt {
            text: "describe [Image #1]".into(),
            attachments: vec![attachment],
        };

        let blocks = prompt_blocks(&prompt).unwrap();

        let ContentBlock::Text(text) = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.text.starts_with("describe [Image #1](file://"));
        let ContentBlock::Image(image) = &blocks[1] else {
            panic!("expected image block");
        };
        assert_eq!(image.mime_type.to_string(), "image/png");
        assert_eq!(image.data, "cG5n");
        assert!(
            image
                .uri
                .as_deref()
                .is_some_and(|uri| uri.starts_with("file://"))
        );
    }

    #[test]
    fn audio_attachment_is_resolved_into_an_acp_prompt_block() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audio.wav");
        std::fs::write(&path, b"wav").unwrap();
        let mut attachment = attachments_from_paste(directory.path(), path.to_str().unwrap())
            .unwrap()
            .remove(0);
        attachment.placeholder = "[Audio #1]".into();

        let blocks = prompt_blocks(&SubmittedPrompt {
            text: "transcribe [Audio #1]".into(),
            attachments: vec![attachment],
        })
        .unwrap();

        let ContentBlock::Audio(audio) = &blocks[1] else {
            panic!("expected audio block");
        };
        assert_eq!(audio.mime_type.to_string(), "audio/wav");
        assert_eq!(audio.data, "d2F2");
    }

    #[test]
    fn prompt_rechecks_actual_total_attachment_size() {
        let directory = tempfile::tempdir().unwrap();
        let mut attachments = Vec::new();
        for index in 0..3 {
            let path = directory.path().join(format!("{index}.png"));
            std::fs::write(&path, b"small").unwrap();
            let mut attachment = attachments_from_paste(directory.path(), path.to_str().unwrap())
                .unwrap()
                .remove(0);
            attachment.placeholder = format!("[Image #{}]", index + 1);
            attachments.push(attachment);
            std::fs::write(path, vec![0_u8; 8 * 1024 * 1024]).unwrap();
        }

        let error = prompt_blocks(&SubmittedPrompt {
            text: "images".into(),
            attachments,
        })
        .unwrap_err();

        assert_eq!(error, "attachments exceed the 20 MiB total limit");
    }

    #[test]
    fn user_image_payload_is_structured_without_a_duplicate_label() {
        let uri = "file:///tmp/image.png";
        let (text, images) = user_message_of(vec![
            ContentBlock::Text(TextContent::new(format!("describe [Image #1]({uri})"))),
            ContentBlock::Image(
                agent_client_protocol::schema::v2::ImageContent::new("AQID", "image/png")
                    .uri(Some(uri.into())),
            ),
        ]);

        assert_eq!(text, format!("describe [Image #1]({uri})"));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, "AQID");
        assert_eq!(images[0].mime_type, "image/png");
    }

    #[test]
    fn user_image_blocks_preserve_text_image_text_order() {
        let uri = "file:///tmp/image.png";
        let (text, images) = user_message_of(vec![
            ContentBlock::Text(TextContent::new("before")),
            ContentBlock::Image(
                agent_client_protocol::schema::v2::ImageContent::new("AQID", "image/png")
                    .uri(Some(uri.into())),
            ),
            ContentBlock::Text(TextContent::new("after")),
        ]);

        assert_eq!(text, format!("before\n[Image #1]({uri})\nafter"));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].line, 1);
    }

    #[test]
    fn image_deduplication_requires_the_exact_trusted_link() {
        let actual = "file:///tmp/actual.png";
        let (text, images) = user_message_of(vec![
            ContentBlock::Text(TextContent::new("[Image #1](file:///tmp/different.png)")),
            ContentBlock::Image(
                agent_client_protocol::schema::v2::ImageContent::new("AQID", "image/png")
                    .uri(Some(actual.into())),
            ),
        ]);

        assert_eq!(
            text,
            format!("[Image #1](file:///tmp/different.png)\n[Image #1]({actual})")
        );
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].line, 1);
    }

    #[test]
    fn rendered_image_never_exposes_a_data_url() {
        let content = ContentBlock::Image(
            agent_client_protocol::schema::v2::ImageContent::new("c2VjcmV0", "image/png")
                .uri(Some("data:image/png;base64,c2VjcmV0".into())),
        );

        assert_eq!(message_of(content).as_deref(), Some("[Image]"));
    }

    #[test]
    fn ordinary_paste_is_not_treated_as_an_attachment() {
        assert!(attachments_from_paste(PathBuf::from(".").as_path(), "hello world").is_none());
    }

    #[test]
    fn saves_defaults_by_parsing_and_reserializing_valid_toml() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#""provider" = "old"
model = "old"
message = """
a = [still text]
"""

[custom]
"quoted.key" = "preserved"
"#,
        )
        .unwrap();
        let choice = ModelChoice {
            id: "openrouter:anthropic/claude-sonnet-4".into(),
            provider: "openrouter".into(),
            model: "anthropic/claude-sonnet-4".into(),
        };

        save_model_defaults_to(&path, &choice).unwrap();

        let saved = toml::from_str::<toml::Value>(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved["provider"].as_str(), Some("openrouter"));
        assert_eq!(saved["model"].as_str(), Some("anthropic/claude-sonnet-4"));
        assert_eq!(saved["message"].as_str(), Some("a = [still text]\n"));
        assert_eq!(saved["custom"]["quoted.key"].as_str(), Some("preserved"));
    }

    #[test]
    fn saves_and_removes_reasoning_effort_without_losing_other_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "model = \"kept\"\n[custom]\nvalue = 7\n").unwrap();

        save_effort_default_to(&path, "high").unwrap();
        let saved =
            toml::from_str::<toml::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["reasoning_effort"].as_str(), Some("high"));
        assert_eq!(saved["custom"]["value"].as_integer(), Some(7));

        save_effort_default_to(&path, "default").unwrap();
        let saved =
            toml::from_str::<toml::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.get("reasoning_effort").is_none());
        assert_eq!(saved["model"].as_str(), Some("kept"));
    }

    #[test]
    fn reads_advertised_reasoning_effort_state() {
        let options = vec![SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning effort",
            "medium",
            vec![SessionConfigSelectGroup::new(
                "reasoning-effort",
                "Reasoning effort",
                vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("medium", "Medium"),
                ],
            )],
        )];

        let (current, choices) = effort_state(Some(&options)).unwrap();
        assert_eq!(current, "medium");
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.id.as_str())
                .collect::<Vec<_>>(),
            ["default", "medium"]
        );
    }

    #[test]
    fn refreshes_model_and_effort_from_one_config_snapshot() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "openrouter:new-model",
                vec![SessionConfigSelectGroup::new(
                    "openrouter",
                    "OpenRouter",
                    vec![SessionConfigSelectOption::new(
                        "openrouter:new-model",
                        "new-model",
                    )],
                )],
            ),
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning effort",
                "high",
                vec![SessionConfigSelectGroup::new(
                    "reasoning-effort",
                    "Reasoning effort",
                    vec![SessionConfigSelectOption::new("high", "High")],
                )],
            ),
        ];
        let mut app = App::new(
            PathBuf::from("."),
            "old-provider".into(),
            "old-model".into(),
            "a2a".into(),
        );

        refresh_config_state(&mut app, Some(&options));

        assert_eq!(app.provider, "openrouter");
        assert_eq!(app.model, "new-model");
        assert_eq!(app.reasoning_effort, "high");
        assert_eq!(app.model_choices.len(), 1);
        assert_eq!(app.effort_choices.len(), 1);
    }

    #[test]
    fn reads_the_current_model_from_new_session_options() {
        let options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "openrouter:anthropic/claude-sonnet-4",
            vec![SessionConfigSelectGroup::new(
                "openrouter",
                "OpenRouter",
                vec![SessionConfigSelectOption::new(
                    "openrouter:anthropic/claude-sonnet-4",
                    "anthropic/claude-sonnet-4",
                )],
            )],
        )];

        let choice = current_model_choice(Some(&options)).unwrap();

        assert_eq!(choice.provider, "openrouter");
        assert_eq!(choice.model, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn encodes_exact_text_for_the_terminal_clipboard() {
        assert_eq!(
            osc52("# hi\n\tthere  "),
            "\x1b]52;c;IyBoaQoJdGhlcmUgIA==\x07"
        );
    }

    #[test]
    fn forwards_all_resolved_telemetry_settings_to_the_tui_child() {
        let settings = crate::telemetry::Settings::try_new(
            Some("http://collector:4317".into()),
            false,
            12,
            4096,
        )
        .unwrap();
        let mut command = tokio::process::Command::new("kit");
        settings.append_cli_args(&mut command);
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--otel-endpoint",
                "http://collector:4317",
                "--otel-capture-message-content",
                "false",
                "--otel-message-content-max-messages",
                "12",
                "--otel-message-content-max-bytes",
                "4096",
            ]
        );
    }

    #[test]
    fn uses_the_validated_acp_id_as_the_durable_session_id() {
        assert_eq!(
            durable_session_id(&wire::SessionId::new("s-123-4-5")).unwrap(),
            "s-123-4-5"
        );
        assert!(durable_session_id(&wire::SessionId::new("bad/id")).is_err());
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
    use std::{future, time::Duration};

    use super::{Stop, bounded_graceful_close};

    #[tokio::test]
    async fn a_stuck_close_is_bounded() {
        let closed = bounded_graceful_close(
            future::pending::<()>(),
            future::pending(),
            Duration::from_millis(1),
        )
        .await;
        assert!(closed.is_none());
    }

    #[tokio::test]
    async fn a_second_stop_escapes_a_stuck_close() {
        let closed = bounded_graceful_close(
            future::pending::<()>(),
            future::ready(()),
            Duration::from_secs(60),
        )
        .await;
        assert!(closed.is_none());
    }

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
