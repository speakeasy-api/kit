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
    terminal::EnterAlternateScreen,
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
    protocols::acp::{
        FileSearchRequest, MODEL_CONFIG_ID, REASONING_EFFORT_CONFIG_ID, model_switch,
    },
    tools::mcp::CredentialStorage,
};

use app::{
    Action, App, Attachment, AttachmentKind, EffortChoice, ModelChoice, SubmittedPrompt, Update,
    UserImage,
};

struct ModelSwitchCompletion {
    generation: u64,
    operation: u64,
    response: Result<wire::SetSessionConfigOptionResponse, agent_client_protocol::Error>,
}

fn prepare_model_switch_request(
    app: &mut App,
    action: Action,
    session_id: wire::SessionId,
) -> Result<(u64, SetSessionConfigOptionRequest), agent_client_protocol::Error> {
    let confirmation = match action {
        Action::SelectModel {
            choice,
            save_defaults,
        } => {
            app.begin_model_switch(choice, save_defaults)
                .ok_or_else(|| {
                    agent_client_protocol::util::internal_error(
                        "wait for the current operation before changing models",
                    )
                })?;
            None
        }
        Action::ConfirmModelSwitch(decision) => {
            let warning = app
                .model_switch
                .as_ref()
                .and_then(|pending| pending.warning.as_ref())
                .ok_or_else(|| {
                    agent_client_protocol::util::internal_error(
                        "no model-switch warning to confirm",
                    )
                })?;
            Some(
                serde_json::to_value(model_switch::Confirmation {
                    token: warning.token,
                    action: decision,
                })
                .map_err(agent_client_protocol::Error::into_internal_error)?,
            )
        }
        _ => {
            return Err(agent_client_protocol::util::internal_error(
                "invalid model-switch action",
            ));
        }
    };
    let pending = app
        .model_switch
        .as_mut()
        .ok_or_else(|| agent_client_protocol::util::internal_error("no pending model switch"))?;
    let mut request =
        SetSessionConfigOptionRequest::new(session_id, MODEL_CONFIG_ID, pending.choice.id.as_str());
    if let Some(confirmation) = confirmation {
        request.meta = Some(serde_json::Map::from_iter([(
            model_switch::META.into(),
            confirmation,
        )]));
        // Keep the warning available if preparing its confirmation fails.
        pending.warning = None;
    }
    Ok((pending.id, request))
}

fn take_model_switch_completion(
    app: &mut App,
    route: &Arc<Mutex<ActiveSessionRoute>>,
    generation: u64,
    operation: u64,
) -> Result<Option<app::ModelSwitch>, agent_client_protocol::Error> {
    let route = route.lock().map_err(|_| {
        agent_client_protocol::util::internal_error("active session route poisoned")
    })?;
    if route.generation != generation
        || app
            .model_switch
            .as_ref()
            .is_none_or(|pending| pending.id != operation)
    {
        return Ok(None);
    }
    Ok(app.model_switch.take())
}

/// Animation and elapsed-time refresh interval.
const TICK: Duration = Duration::from_millis(90);
/// Terminal events or queued updates applied per frame.
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

fn apply_pending_updates(
    app: &mut App,
    route: &Arc<Mutex<ActiveSessionRoute>>,
    updates: &mut mpsc::UnboundedReceiver<QueuedUpdate>,
    first: QueuedUpdate,
) {
    for queued in std::iter::once(first)
        .chain(std::iter::from_fn(|| updates.try_recv().ok()))
        .take(MAX_BURST)
    {
        let Some(update) = accept_queued_update(route, queued) else {
            continue;
        };
        if let Update::ConfigOptions(options) = &update {
            refresh_config_state(app, Some(options));
        }
        app.apply(update);
    }
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
const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

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

fn error_detail(error: &agent_client_protocol::Error) -> &str {
    match error.data.as_ref() {
        Some(Value::String(detail)) => detail,
        Some(data) => crate::protocols::acp::AuthenticationRequiredData::from_value(data)
            .map(|required| required.detail)
            .unwrap_or(&error.message),
        _ => &error.message,
    }
}

fn authentication_required(
    error: &agent_client_protocol::Error,
    methods: &[wire::AuthMethodTerminal],
) -> bool {
    if error.code != agent_client_protocol::ErrorCode::AuthRequired {
        return false;
    }
    error
        .data
        .as_ref()
        .and_then(crate::protocols::acp::AuthenticationRequiredData::from_value)
        .is_none_or(|required| {
            methods
                .iter()
                .any(|method| method.method_id.0.as_ref() == required.method_id)
        })
}

fn credential_storage_for_launch(
    credential_storage: &CredentialStorage,
    current_dir: impl FnOnce() -> std::io::Result<PathBuf>,
) -> std::io::Result<CredentialStorage> {
    match credential_storage {
        CredentialStorage::Filesystem(directory) if directory.is_relative() => Ok(
            CredentialStorage::Filesystem(current_dir()?.join(directory)),
        ),
        credential_storage => Ok(credential_storage.clone()),
    }
}

fn client_capabilities(credential_storage: &CredentialStorage) -> wire::ClientCapabilities {
    let capabilities = wire::ClientCapabilities::new();
    if credential_storage.is_persistent() {
        capabilities
            .auth(wire::AuthCapabilities::new().terminal(wire::TerminalAuthCapabilities::new()))
    } else {
        capabilities
    }
}

fn usable_terminal_auth_methods(methods: &[wire::AuthMethod]) -> Vec<wire::AuthMethodTerminal> {
    methods
        .iter()
        .filter_map(|method| match method {
            wire::AuthMethod::Terminal(method) if !method.method_id.0.is_empty() => {
                Some(method.clone())
            }
            _ => None,
        })
        .collect()
}

#[derive(Clone)]
struct AgentInvocation {
    program: std::ffi::OsString,
    args: Vec<std::ffi::OsString>,
    env: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    current_dir: Option<PathBuf>,
}

impl AgentInvocation {
    fn from_command(command: &std::process::Command) -> Self {
        Self {
            program: command.get_program().to_owned(),
            args: command.get_args().map(std::ffi::OsStr::to_owned).collect(),
            env: command
                .get_envs()
                .filter(|(name, _)| *name != std::ffi::OsStr::new(OPENROUTER_API_KEY_ENV))
                .map(|(name, value)| (name.to_owned(), value.map(std::ffi::OsStr::to_owned)))
                .collect(),
            current_dir: command.get_current_dir().map(Path::to_path_buf),
        }
    }

    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args);
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        for (name, value) in &self.env {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
    }
}

#[allow(clippy::too_many_arguments)]
fn agent_command_for_launch(
    root: &Path,
    model: &str,
    provider: crate::ProviderKind,
    reasoning_effort: Option<crate::ReasoningEffort>,
    openrouter_api_key: Option<&crate::provider::OpenRouterApiKey>,
    a2a: Option<&str>,
    mcp_config: Option<&Path>,
    telemetry: &crate::telemetry::Settings,
    credential_storage: &CredentialStorage,
    session_id: &str,
    resume: bool,
    force: bool,
) -> std::io::Result<tokio::process::Command> {
    let mut command = crate::acp_child::serve_command(
        root,
        model,
        provider,
        reasoning_effort,
        openrouter_api_key,
        session_id,
        resume,
    )?;
    if let Some(address) = a2a {
        command.arg("--a2a").arg(address);
    }
    if let Some(path) = mcp_config {
        command.arg("--mcp-config").arg(path);
    }
    telemetry.append_cli_args(&mut command);
    credential_storage.append_cli_args(&mut command);
    if force {
        command.arg("--force");
    }
    Ok(command)
}

fn terminal_auth_command(
    invocation: &AgentInvocation,
    root: &Path,
    method: &wire::AuthMethodTerminal,
) -> tokio::process::Command {
    let mut command = invocation.command();
    command
        .env_remove(OPENROUTER_API_KEY_ENV)
        .args(&method.args)
        .current_dir(root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for variable in &method.env {
        command.env(&variable.name, &variable.value);
    }
    command
}

fn authenticate_in_terminal<'a>(
    invocation: &AgentInvocation,
    root: &Path,
    method: &wire::AuthMethodTerminal,
    stop: &'a mut Stop,
) -> impl std::future::Future<Output = Option<std::io::Result<std::process::ExitStatus>>> + 'a {
    wait_for_terminal_auth(terminal_auth_command(invocation, root, method), stop)
}

enum ConnectedAuthentication {
    Completed(Option<std::io::Result<std::process::ExitStatus>>),
    AgentExited(Result<std::io::Result<std::process::ExitStatus>, oneshot::error::RecvError>),
    UpdatesClosed,
}

async fn wait_for_connected_authentication(
    authentication: impl std::future::Future<Output = Option<std::io::Result<std::process::ExitStatus>>>,
    prepare: impl FnOnce(),
    app: &mut App,
    route: &Arc<Mutex<ActiveSessionRoute>>,
    updates: &mut mpsc::UnboundedReceiver<QueuedUpdate>,
    exit: &mut oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
) -> ConnectedAuthentication {
    prepare();
    tokio::pin!(authentication);
    loop {
        tokio::select! {
            authenticated = &mut authentication => {
                return ConnectedAuthentication::Completed(authenticated);
            }
            update = updates.recv() => match update {
                Some(update) => apply_pending_updates(app, route, updates, update),
                None => return ConnectedAuthentication::UpdatesClosed,
            },
            status = &mut *exit => return ConnectedAuthentication::AgentExited(status),
        }
    }
}

async fn wait_for_terminal_auth(
    mut command: tokio::process::Command,
    stop: &mut Stop,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    let child = command.kill_on_drop(true).spawn();
    let Ok(mut child) = child else {
        return Some(child.map(|_| unreachable!()));
    };
    tokio::select! {
        status = child.wait() => Some(status),
        () = stop.requested() => {
            let _ = child.kill().await;
            None
        }
    }
}

enum RequestInterrupt {
    AgentExited(Option<std::process::ExitStatus>),
    Stopped,
    TimedOut,
}

enum RequestFailure {
    AgentExited(Option<std::process::ExitStatus>),
    TimedOut,
}

async fn bounded_agent_request<T>(
    request: impl std::future::Future<Output = T>,
    exit: &mut oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
    stop: impl std::future::Future<Output = ()>,
    timeout: Duration,
) -> Result<T, RequestInterrupt> {
    tokio::select! {
        output = request => Ok(output),
        status = &mut *exit => Err(RequestInterrupt::AgentExited(
            status.ok().and_then(Result::ok),
        )),
        () = stop => Err(RequestInterrupt::Stopped),
        () = tokio::time::sleep(timeout) => Err(RequestInterrupt::TimedOut),
    }
}

async fn bounded_startup_request<T>(
    request: impl std::future::Future<Output = T>,
    exit: &mut oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
    timeout: Duration,
) -> Result<T, RequestFailure> {
    match bounded_agent_request(request, exit, std::future::pending(), timeout).await {
        Ok(output) => Ok(output),
        Err(RequestInterrupt::AgentExited(status)) => Err(RequestFailure::AgentExited(status)),
        Err(RequestInterrupt::TimedOut) => Err(RequestFailure::TimedOut),
        Err(RequestInterrupt::Stopped) => unreachable!("the stop future is pending"),
    }
}

async fn bounded_cancellable_request<T>(
    request: impl std::future::Future<Output = T>,
    exit: &mut oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
    stop: impl std::future::Future<Output = ()>,
    timeout: Duration,
) -> Result<Option<T>, RequestFailure> {
    match bounded_agent_request(request, exit, stop, timeout).await {
        Ok(output) => Ok(Some(output)),
        Err(RequestInterrupt::AgentExited(status)) => Err(RequestFailure::AgentExited(status)),
        Err(RequestInterrupt::Stopped) => Ok(None),
        Err(RequestInterrupt::TimedOut) => Err(RequestFailure::TimedOut),
    }
}

async fn request_failure(
    failure: RequestFailure,
    stderr: tokio::task::JoinHandle<()>,
    recent: &Mutex<Vec<String>>,
    when: &str,
    timeout_message: String,
) -> agent_client_protocol::Error {
    let detail = match failure {
        RequestFailure::AgentExited(status) => died(status, stderr, recent, when).await,
        RequestFailure::TimedOut => timeout_message,
    };
    agent_client_protocol::Error::into_internal_error(std::io::Error::other(detail))
}

async fn request_initial_session(
    connection: &agent_client_protocol::V2ConnectionTo<agent_client_protocol::Agent>,
    resume_id: Option<&str>,
    root: &Path,
) -> Result<(wire::SessionId, Vec<SessionConfigOption>), agent_client_protocol::Error> {
    if let Some(resume_id) = resume_id {
        let response = connection
            .send_request(
                wire::ResumeSessionRequest::new(resume_id, root.to_path_buf())
                    .replay_from(wire::ReplayFrom::Start(wire::ReplayFromStart::new())),
            )
            .block_task()
            .await?;
        Ok((wire::SessionId::new(resume_id), response.config_options))
    } else {
        let response = connection
            .send_request(wire::NewSessionRequest::new(root.to_path_buf()))
            .block_task()
            .await?;
        Ok((response.session_id, response.config_options))
    }
}

fn current_model_choice(options: Option<&[SessionConfigOption]>) -> Option<ModelChoice> {
    let current = current_config_value(options.unwrap_or_default(), MODEL_CONFIG_ID)?;
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
        .find(|option| option.config_id.to_string() == REASONING_EFFORT_CONFIG_ID)?
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

struct ActiveSessionConfig {
    model: String,
    reasoning_effort: Option<String>,
}

fn active_session_config(app: &App) -> ActiveSessionConfig {
    ActiveSessionConfig {
        model: app
            .model_choices
            .iter()
            .find(|choice| choice.provider == app.provider && choice.model == app.model)
            .map_or_else(
                || format!("{}:{}", app.provider, app.model),
                |choice| choice.id.clone(),
            ),
        reasoning_effort: (!app.effort_choices.is_empty()).then(|| app.reasoning_effort.clone()),
    }
}

fn current_config_value(options: &[SessionConfigOption], config_id: &str) -> Option<String> {
    options
        .iter()
        .find(|option| option.config_id.to_string() == config_id)
        .and_then(|option| match &option.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
            _ => None,
        })
}

fn active_config_matches(active: &ActiveSessionConfig, options: &[SessionConfigOption]) -> bool {
    current_config_value(options, MODEL_CONFIG_ID).as_deref() == Some(active.model.as_str())
        && active.reasoning_effort.as_deref().is_none_or(|effort| {
            current_config_value(options, REASONING_EFFORT_CONFIG_ID).as_deref() == Some(effort)
        })
}

async fn refresh_session_after_auth(
    connection: &agent_client_protocol::V2ConnectionTo<agent_client_protocol::Agent>,
    session_id: wire::SessionId,
    active: &ActiveSessionConfig,
) -> Result<Vec<SessionConfigOption>, agent_client_protocol::Error> {
    let options = connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id,
            MODEL_CONFIG_ID,
            active.model.as_str(),
        ))
        .block_task()
        .await?
        .config_options;
    if !active_config_matches(active, &options) {
        return Err(agent_client_protocol::Error::into_internal_error(
            std::io::Error::other("authentication refresh changed the active session settings"),
        ));
    }
    Ok(options)
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
        .find(|option| option.config_id.to_string() == MODEL_CONFIG_ID)
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
    let credential_storage =
        credential_storage_for_launch(credential_storage, std::env::current_dir)?;
    let credential_storage = &credential_storage;
    let resume_session_id = resume.map(str::to_string);
    let persisted_session_id = resume_session_id
        .clone()
        .unwrap_or_else(crate::session::new_id);
    let active_persisted_id = Arc::new(Mutex::new(ActiveSessionRoute {
        id: persisted_session_id.clone(),
        generation: 0,
    }));
    let root = root.to_path_buf();
    let model = model.to_string();
    let a2a_address = a2a.map(str::to_string);
    let a2a = a2a_address.clone().unwrap_or_else(|| "allocating…".into());
    let mcp_config = mcp_config.map(Path::to_path_buf);

    {
        let launch_session_id = resume_session_id
            .as_deref()
            .unwrap_or(&persisted_session_id);
        let mut command = agent_command_for_launch(
            &root,
            &model,
            provider,
            reasoning_effort,
            openrouter_api_key,
            a2a_address.as_deref(),
            mcp_config.as_deref(),
            telemetry,
            credential_storage,
            launch_session_id,
            resume_session_id.is_some(),
            force,
        )?;
        let child_mcp_config = mcp_config.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
                crate::resilient_fs::global()
                    .require_disk(PathBuf::from(home).join(".kit/config.toml"))?;
            }
            if let Some(path) = child_mcp_config {
                crate::resilient_fs::global().require_disk(path)?;
            }
            Ok::<_, std::io::Error>(())
        })
        .await??;
        let auth_invocation = AgentInvocation::from_command(command.as_std());
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
        let (exit_tx, mut exit_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let watcher = tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => {
                    let notification = status.as_ref().copied().map_err(|error| {
                        std::io::Error::new(error.kind(), error.to_string())
                    });
                    let _ = exit_tx.send(notification);
                    status
                }
                _ = shutdown_rx => {
                    // ACP EOF makes serve stop A2A, drain sessions, and run
                    // final storage recovery before exiting.
                    wait_for_storage_exit(&mut child, Duration::from_secs(10)).await
                }
            }?;
            if !status.success() {
                return Err(std::io::Error::other(format!(
                    "agent exited with {status}; final storage recovery may have failed; unpersisted data may have been lost"
                )));
            }
            Ok::<_, std::io::Error>(())
        });

        let notifications = updates_tx.clone();
        let cleanup_root = root.clone();
        let transition_session = Arc::clone(&active_persisted_id);
        let notification_session = Arc::clone(&active_persisted_id);
        let result = {
            let root = root.clone();
            let model = model.clone();
            let a2a = a2a.clone();
            let auth_invocation = auth_invocation.clone();
            let resume_session_id = resume_session_id.clone();
            agent_client_protocol::Client
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
            let initialize = connection
                .send_request(
                    wire::InitializeRequest::new(
                        ProtocolVersion::V2,
                        wire::Implementation::new(
                            "kit-tui",
                            env!("CARGO_PKG_VERSION"),
                        ),
                    )
                    .capabilities(client_capabilities(credential_storage)),
                )
                .block_task();
            let initialized =
                match bounded_startup_request(initialize, &mut exit_rx, HANDSHAKE).await {
                    Ok(initialized) => initialized?,
                    Err(failure) => {
                        return Err(request_failure(
                            failure,
                            stderr_task,
                            &recent,
                            "before the session opened",
                            format!(
                                "the agent did not answer the ACP handshake within {} seconds",
                                HANDSHAKE.as_secs()
                            ),
                        )
                        .await);
                    }
                };
            if initialized.protocol_version != ProtocolVersion::V2 {
                return Err(agent_client_protocol::Error::into_internal_error(
                    std::io::Error::other("the agent did not negotiate ACP v2"),
                ));
            }
            let auth_methods = usable_terminal_auth_methods(&initialized.auth_methods);
            let can_steer = initialized.capabilities.session.as_ref()
                .and_then(|session| session.inject.as_ref())
                .is_some_and(|inject| {
                    inject.modes.contains(&wire::SessionInjectMode::Steer)
                        && inject.steer_in_stream.as_ref().is_some_and(|modes| {
                            modes.contains(&wire::SessionInjectSteerInStream::Finish)
                        })
                });

            let initial = match bounded_startup_request(
                request_initial_session(
                    &connection,
                    resume_session_id.as_deref(),
                    &root,
                ),
                &mut exit_rx,
                HANDSHAKE,
            )
            .await
            {
                Ok(session) => session,
                Err(failure) => {
                    return Err(request_failure(
                        failure,
                        stderr_task,
                        &recent,
                        "before the session opened",
                        format!(
                            "the agent did not start a session within {} seconds",
                            HANDSHAKE.as_secs()
                        ),
                    )
                    .await);
                }
            };
            let initial = match initial {
                Err(error)
                    if auth_methods.is_empty()
                        || !authentication_required(&error, &auth_methods) =>
                {
                    return Err(error);
                }
                result => result,
            };

            // Install every fallible signal handler before changing terminal modes so
            // an installation failure cannot leave the caller's terminal altered.
            let mut stop =
                Stop::new().map_err(agent_client_protocol::Error::into_internal_error)?;
            let (mut terminal, mut images) =
                enter().map_err(agent_client_protocol::Error::into_internal_error)?;
            let mut app = App::new(
                root.clone(),
                provider.as_str().to_string(),
                model.clone(),
                a2a,
            );
            app.can_steer = can_steer;
            app.can_replace_steer = supports_pending_replace(
                initialized.capabilities.session.as_ref().and_then(|session| session.inject.as_ref()),
            );
            app.auth_methods = auth_methods;
            let mut events = EventStream::new();
            let mut ticker = tokio::time::interval(TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            let (mut session_id, config_options) = match initial {
                Ok(session) => session,
                Err(session_error) => {
                    app.note(format!(
                        "could not start the session: {}; use /login to authenticate",
                        error_detail(&session_error)
                    ));
                    loop {
                    if let Err(error) =
                        terminal.draw(|frame| ui::draw(frame, &mut app, &mut images))
                    {
                        leave(&mut terminal);
                        return Err(agent_client_protocol::Error::into_internal_error(error));
                    }
                    tokio::select! {
                        event = events.next() => {
                            let action = match event {
                                Some(Ok(event)) => handle(&mut app, event),
                                Some(Err(_)) | None => {
                                    leave(&mut terminal);
                                    return Ok(());
                                }
                            };
                            match action {
                                Action::Quit => {
                                    leave(&mut terminal);
                                    return Ok(());
                                }
                                Action::Login(method) => {
                                    drop(events);
                                    let authentication = authenticate_in_terminal(
                                        &auth_invocation,
                                        &root,
                                        &method,
                                        &mut stop,
                                    );
                                    let authenticated = wait_for_connected_authentication(
                                        authentication,
                                        || {
                                            leave(&mut terminal);
                                            println!("Starting {}…", method.name);
                                        },
                                        &mut app,
                                        &transition_session,
                                        &mut updates_rx,
                                        &mut exit_rx,
                                    )
                                    .await;
                                    let authenticated = match authenticated {
                                        ConnectedAuthentication::Completed(authenticated) => authenticated,
                                        ConnectedAuthentication::UpdatesClosed => return Ok(()),
                                        ConnectedAuthentication::AgentExited(status) => {
                                            return Err(agent_client_protocol::Error::into_internal_error(
                                                std::io::Error::other(died(
                                                    status.ok().and_then(Result::ok),
                                                    stderr_task,
                                                    &recent,
                                                    "before the session opened",
                                                ).await),
                                            ));
                                        }
                                    };
                                    let Some(outcome) = authenticated else {
                                        return Ok(());
                                    };
                                    match outcome {
                                        Ok(status) if status.success() => {
                                            let requested = bounded_cancellable_request(
                                                request_initial_session(
                                                    &connection,
                                                    resume_session_id.as_deref(),
                                                    &root,
                                                ),
                                                &mut exit_rx,
                                                stop.requested(),
                                                HANDSHAKE,
                                            )
                                            .await;
                                            let Some(initial) = (match requested {
                                                Ok(initial) => initial,
                                                Err(failure) => {
                                                    return Err(request_failure(
                                                        failure,
                                                        stderr_task,
                                                        &recent,
                                                        "before the session opened",
                                                        format!(
                                                            "the agent did not start a session within {} seconds",
                                                            HANDSHAKE.as_secs()
                                                        ),
                                                    )
                                                    .await);
                                                }
                                            }) else {
                                                return Ok(());
                                            };
                                            match initial {
                                                Ok(session) => {
                                                    images = resume_terminal(&mut terminal).map_err(
                                                        agent_client_protocol::Error::into_internal_error,
                                                    )?;
                                                    events = EventStream::new();
                                                    break session;
                                                }
                                                Err(error) if authentication_required(
                                                    &error,
                                                    &app.auth_methods,
                                                ) => app.note(format!(
                                                    "could not start the session: {}; use /login to authenticate",
                                                    error_detail(&error)
                                                )),
                                                Err(error) => return Err(error),
                                            }
                                        }
                                        Ok(status) => app.note(format!(
                                            "authentication with {} failed: {status}",
                                            method.name
                                        )),
                                        Err(error) => app.note(format!(
                                            "could not start authentication with {}: {error}",
                                            method.name
                                        )),
                                    }
                                    images = resume_terminal(&mut terminal).map_err(
                                        agent_client_protocol::Error::into_internal_error,
                                    )?;
                                    events = EventStream::new();
                                }
                                Action::None | Action::Redraw => {}
                                _ => app.note("authenticate before starting a session"),
                            }
                        }
                        update = updates_rx.recv() => match update {
                            Some(update) => app.apply(update.update),
                            None => {
                                leave(&mut terminal);
                                return Ok(());
                            }
                        },
                        _ = ticker.tick(), if app.needs_redraw_tick() => app.tick(),
                        () = stop.requested() => {
                            leave(&mut terminal);
                            return Ok(());
                        }
                    }
                    }
                },
            };
            let active_session_id = match durable_session_id(&session_id) {
                Ok(session_id) => session_id,
                Err(error) => {
                    leave(&mut terminal);
                    return Err(agent_client_protocol::Error::into_internal_error(
                        std::io::Error::other(error),
                    ));
                }
            };
            refresh_config_state(&mut app, Some(&config_options));
            let mut saved_model_default = current_model_choice(Some(&config_options));
            let (switch_tx, mut switch_rx) = mpsc::unbounded_channel::<ModelSwitchCompletion>();
            if let Ok(mut active) = transition_session.lock() {
                active.id = active_session_id.clone();
            }
            app.start_session(active_session_id.clone());
            let storage_shutdown = crate::resilient_fs::shutdown_token();
            let result: Result<(), agent_client_protocol::Error> = async {
                loop {
                    terminal
                        .draw(|frame| ui::draw(frame, &mut app, &mut images))
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    tokio::select! {
                        _ = storage_shutdown.cancelled() => return Ok(()),
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
                                    let editable = pending_steer_is_editable(&blocks);
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
                                            editable,
                                        }),
                                        Ok(None) => {}
                                        Err(error) => {
                                            app.paste(&prompt.text);
                                            app.restore_attachments(prompt.attachments);
                                            app.note(format!("message was not accepted: {}", error.message));
                                        }
                                    }
                                }
                                Action::ReplaceSteer { id, text } => {
                                    let Ok(route) = transition_session.lock() else {
                                        app.note("could not start pending-message edit");
                                        continue;
                                    };
                                    let generation = route.generation;
                                    drop(route);
                                    let Some(token) = app.begin_steer_mutation(&id, Some(text.clone())) else { continue; };
                                    let connection = connection.clone();
                                    let session_id = session_id.clone();
                                    let request_id = id.clone();
                                    spawn_steer_mutation(generation, id, token, updates_tx.clone(), async move {
                                        connection
                                            .send_request(wire::ReplaceInjectSessionRequest::new(
                                                session_id,
                                                request_id,
                                                vec![wire::ContentBlock::Text(wire::TextContent::new(text))],
                                            ))
                                            .block_task()
                                            .await
                                            .map(|_| ())
                                    });
                                }
                                Action::RevokeSteer { id } => {
                                    let Ok(route) = transition_session.lock() else {
                                        app.note("could not start pending-message removal");
                                        continue;
                                    };
                                    let generation = route.generation;
                                    drop(route);
                                    let Some(token) = app.begin_steer_mutation(&id, None) else { continue; };
                                    let connection = connection.clone();
                                    let session_id = session_id.clone();
                                    let request_id = id.clone();
                                    spawn_steer_mutation(generation, id, token, updates_tx.clone(), async move {
                                        connection
                                            .send_request(wire::RevokeInjectSessionRequest::new(session_id, request_id))
                                            .block_task()
                                            .await
                                            .map(|_| ())
                                    });
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
                                    let config_options = if let Some(choice) = &saved_model_default {
                                        connection
                                            .send_request(SetSessionConfigOptionRequest::new(
                                                session_id.clone(),
                                                MODEL_CONFIG_ID,
                                                choice.id.as_str(),
                                            ))
                                            .block_task()
                                            .await?
                                            .config_options
                                    } else {
                                        session.config_options
                                    };
                                    let persisted_id = durable_session_id(&session_id).map_err(|error| {
                                        agent_client_protocol::Error::into_internal_error(std::io::Error::other(error))
                                    })?;
                                    transition_route(&transition_session, persisted_id.clone());
                                    images.clear();
                                    app.start_session(persisted_id);
                                    refresh_config_state(&mut app, Some(&config_options));
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
                                action @ (Action::SelectModel { .. } | Action::ConfirmModelSwitch(_)) => {
                                    // A poisoned route may pair a session ID with the wrong generation.
                                    // Stop explicitly rather than send a request with untrusted correlation.
                                    let generation = transition_session.lock().map_err(|_| {
                                        agent_client_protocol::util::internal_error("active session route poisoned")
                                    })?.generation;
                                    let (operation, request) = match prepare_model_switch_request(&mut app, action, session_id.clone()) {
                                        Ok(request) => request,
                                        Err(error) => {
                                            app.note(format!("could not change model: {}", error.message));
                                            continue;
                                        }
                                    };
                                    let connection = connection.clone();
                                    let completed = switch_tx.clone();
                                    tokio::task::spawn_local(async move {
                                        let response = connection.send_request(request).block_task().await;
                                        let _ = completed.send(ModelSwitchCompletion { generation, operation, response });
                                    });
                                }
                                Action::SelectEffort { effort, save_defaults } => {
                                    let response = connection
                                        .send_request(SetSessionConfigOptionRequest::new(
                                            session_id.clone(),
                                            REASONING_EFFORT_CONFIG_ID,
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
                                                let saved = {
                                                    let effort = effort.clone();
                                                    tokio::task::spawn_blocking(move || save_effort_default(&effort)).await.map_err(|error| error.to_string()).and_then(|result| result)
                                                };
                                                match saved {
                                                    Ok(()) => app.note(
                                                        config_save_message("reasoning effort default"),
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
                                Action::Login(method) => {
                                    drop(events);
                                    let active_config = active_session_config(&app);
                                    let authentication = authenticate_in_terminal(
                                        &auth_invocation,
                                        &root,
                                        &method,
                                        &mut stop,
                                    );
                                    let authenticated = wait_for_connected_authentication(
                                        authentication,
                                        || {
                                            leave(&mut terminal);
                                            println!("Starting {}…", method.name);
                                        },
                                        &mut app,
                                        &transition_session,
                                        &mut updates_rx,
                                        &mut exit_rx,
                                    )
                                    .await;
                                    let authenticated = match authenticated {
                                        ConnectedAuthentication::Completed(authenticated) => authenticated,
                                        ConnectedAuthentication::UpdatesClosed => return Ok(()),
                                        ConnectedAuthentication::AgentExited(status) => {
                                            return Err(agent_client_protocol::Error::into_internal_error(
                                                std::io::Error::other(died(
                                                    status.ok().and_then(Result::ok),
                                                    stderr_task,
                                                    &recent,
                                                    "during authentication",
                                                ).await),
                                            ));
                                        }
                                    };
                                    let Some(outcome) = authenticated else {
                                        return Ok(());
                                    };
                                    match outcome {
                                        Ok(status) if status.success() => {
                                            let refreshed = bounded_cancellable_request(
                                                refresh_session_after_auth(
                                                    &connection,
                                                    session_id.clone(),
                                                    &active_config,
                                                ),
                                                &mut exit_rx,
                                                stop.requested(),
                                                HANDSHAKE,
                                            )
                                            .await;
                                            let Some(options) = (match refreshed {
                                                Ok(options) => options,
                                                Err(failure) => {
                                                    return Err(request_failure(
                                                        failure,
                                                        stderr_task,
                                                        &recent,
                                                        "during authentication refresh",
                                                        format!(
                                                            "the agent did not refresh the session within {} seconds",
                                                            HANDSHAKE.as_secs()
                                                        ),
                                                    )
                                                    .await);
                                                }
                                            }) else {
                                                return Ok(());
                                            };
                                            let options = options?;
                                            refresh_config_state(&mut app, Some(&options));
                                            app.note(format!(
                                                "authentication with {} succeeded",
                                                method.name
                                            ));
                                        }
                                        Ok(status) => app.note(format!(
                                            "authentication with {} failed: {status}",
                                            method.name
                                        )),
                                        Err(error) => app.note(format!(
                                            "could not start authentication with {}: {error}",
                                            method.name
                                        )),
                                    }
                                    images = resume_terminal(&mut terminal).map_err(
                                        agent_client_protocol::Error::into_internal_error,
                                    )?;
                                    events = EventStream::new();
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
                                Action::SearchFiles {
                                    query,
                                    revision,
                                    activation,
                                } => {
                                    let connection = connection.clone();
                                    let updates = updates_tx.clone();
                                    tokio::spawn(async move {
                                        let result = connection
                                            .send_request(FileSearchRequest { query, activation })
                                            .block_task()
                                            .await
                                            .map(|response| response.matches)
                                            .map_err(|error| error.message.to_string());
                                        let _ = updates.send(QueuedUpdate::global(
                                            Update::FileMatches { revision, result },
                                        ));
                                    });
                                }
                                Action::None | Action::Redraw => {}
                            }
                        },
                        Some(completion) = switch_rx.recv() => {
                            let Some(mut pending) = take_model_switch_completion(&mut app, &transition_session, completion.generation, completion.operation)? else { continue; };
                            match completion.response {
                                Ok(response) => {
                                    let choice = pending.choice;
                                    let save_defaults = pending.save_defaults;
                                    app.usage = None;
                                    refresh_config_state(&mut app, Some(&response.config_options));
                                    app.note(format!("model changed to {} via {}", choice.model, choice.provider));
                                    if save_defaults {
                                        let saved = {
                                            let choice = choice.clone();
                                            tokio::task::spawn_blocking(move || save_model_defaults(&choice)).await.map_err(|error| error.to_string()).and_then(|result| result)
                                        };
                                        match saved {
                                            Ok(()) => { saved_model_default = Some(choice); app.note(config_save_message("model defaults")); }
                                            Err(error) => app.note(format!("model changed, but defaults were not saved: {error}")),
                                        }
                                    }
                                }
                                Err(error) => {
                                    let warning = error.data.as_ref().and_then(|data| data.get(model_switch::META)).and_then(|value| serde_json::from_value::<model_switch::Warning>(value.clone()).ok());
                                    if let Some(warning) = warning.filter(|_| !pending.cancelling) {
                                        pending.warning = Some(warning);
                                        app.model_switch = Some(pending);
                                    } else {
                                        app.note(format!("model change failed: {}", error.message));
                                    }
                                }
                            }
                        },
                        update = updates_rx.recv() => match update {
                            Some(update) => apply_pending_updates(
                                &mut app,
                                &transition_session,
                                &mut updates_rx,
                                update,
                            ),
                            None => return Ok(()),
                        },
                        _ = ticker.tick(), if app.needs_redraw_tick() => app.tick(),
                        () = stop.requested() => return Ok(()),
                    }
                }
            }
            .await;
            leave(&mut terminal);
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
        .map_err(explain)
        };

        // The client future has dropped its transport. Allow the storage-owning
        // process to complete graceful shutdown, including its final recovery.
        let _ = shutdown_tx.send(());
        let shutdown_result = watcher.await;
        // If the server failed before acknowledging CloseSession, reclaim only a
        // lock that is now provably stale; a live owner's OS lock is never stolen.
        if let Ok(active) = active_persisted_id.lock() {
            let _ = crate::session::remove_stale_lock(&cleanup_root, &active.id);
        }

        // Keep startup/protocol diagnostics as well as final persistence failure.
        if let Err(shutdown) = shutdown_result? {
            return Err(match result {
                Err(error) => std::io::Error::other(format!("{error}\n{shutdown}")),
                Ok(()) => shutdown,
            }
            .into());
        }
        result?;
        Ok(())
    }
}

async fn wait_for_storage_exit(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            let message = "kit: agent graceful shutdown timed out; forcing termination. Final storage recovery may not have completed; unpersisted data may be lost.";
            eprintln!("{message}");
            child.kill().await?;
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, message))
        }
    }
}

fn config_save_message(setting: &str) -> String {
    if crate::resilient_fs::global().status().pending_operations > 0 {
        format!("updated {setting} in memory; disk persistence is pending (not durable)")
    } else {
        format!("saved {setting} to ~/.kit/config.toml")
    }
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
    let contents = match crate::resilient_fs::read_to_string(path) {
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
    crate::resilient_fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    crate::resilient_fs::replace(path, output.as_bytes())
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

/// Explains an agent exit, quoting the last thing it said.
async fn died(
    status: Option<std::process::ExitStatus>,
    stderr: tokio::task::JoinHandle<()>,
    recent: &Mutex<Vec<String>>,
    when: &str,
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
        format!("the agent {how} {when}")
    } else {
        format!("the agent {how} {when}: {said}")
    }
}

/// Encodes exact source text for the terminal clipboard.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}

/// Await delivery-sensitive ACP mutations off the terminal event loop. Both
/// session generation and the app's mutation token must match at completion.
fn spawn_steer_mutation(
    generation: u64,
    id: String,
    token: u64,
    updates: mpsc::UnboundedSender<QueuedUpdate>,
    request: impl std::future::Future<Output = Result<(), agent_client_protocol::Error>>
    + Send
    + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = request.await.map_err(|error| app::SteerMutationError {
            unavailable: pending_message_unavailable(&error),
            message: format!(
                "pending-message change failed: {}; retry or Esc to restore draft",
                error.message
            ),
        });
        let _ = updates.send(QueuedUpdate::for_session(
            generation,
            Update::SteerMutationFinished { id, token, result },
        ));
    })
}

fn supports_pending_replace(inject: Option<&wire::SessionInjectCapabilities>) -> bool {
    inject
        .and_then(|inject| inject.pending.as_ref())
        .is_some_and(|pending| pending.replace == Some(true))
}

fn pending_message_unavailable(error: &agent_client_protocol::Error) -> bool {
    matches!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(serde_json::Value::as_str),
        Some("already_delivered" | "unknown_message_id")
    )
}

/// Applies one terminal event, returning the work it asks for.
fn handle(app: &mut App, event: Event) -> Action {
    match event {
        Event::Key(key) => app.handle_key(key),
        Event::Mouse(mouse) => app.handle_mouse(mouse),
        Event::Paste(text) => {
            if app.model_switch.is_some() {
                return Action::None;
            }
            if app.queue_focused && !app.session_rename_active() {
                return Action::None;
            }
            if app.session_rename_active() || app.editing_steer() {
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

fn pending_steer_is_editable(blocks: &[ContentBlock]) -> bool {
    blocks
        .iter()
        .all(|block| matches!(block, ContentBlock::Text(_)))
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

fn enable_tui_modes() {
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, EnableBracketedPaste, EnableMouseCapture);
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        ENHANCED.store(true, Ordering::Relaxed);
    }
}

fn enter() -> std::io::Result<(DefaultTerminal, image::ImageRuntime)> {
    TERMINAL_ACTIVE.store(true, Ordering::Relaxed);
    let terminal = ratatui::try_init()?;
    // Query after entering the alternate screen but before the event stream owns
    // terminal input, as required by ratatui-image. The query has a short bound.
    let images = image::ImageRuntime::detect();
    // Bracketed paste keeps pasted newlines out of the key stream. Keyboard
    // enhancement distinguishes command keys, shifted returns, and releases.
    enable_tui_modes();
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

fn resume_terminal(terminal: &mut DefaultTerminal) -> std::io::Result<image::ImageRuntime> {
    TERMINAL_ACTIVE.store(true, Ordering::Relaxed);
    let resumed = (|| {
        crossterm::terminal::enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;
        let images = image::ImageRuntime::detect();
        enable_tui_modes();
        terminal.clear()?;
        Ok(images)
    })();
    if resumed.is_err() {
        restore_modes();
        ratatui::restore();
    }
    resumed
}

/// Avoid terminal escape sequences on protocol stdout when no TUI was entered.
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Best-effort cleanup for actual allocator failure, without command formatting,
/// terminal queries, panic hooks, or allocating our own buffers. Raw-mode cleanup
/// still relies on the platform terminal implementation; arbitrary failures in
/// that implementation cannot be made allocation-safe by this hook.
pub(crate) fn restore_after_allocation_failure() {
    use std::io::Write as _;

    if !TERMINAL_ACTIVE.swap(false, Ordering::Relaxed) {
        return;
    }
    let mut stdout = std::io::stdout();
    if ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = stdout.write_all(b"\x1b[<1u");
    }
    // Mouse modes, bracketed paste, cursor visibility, then alternate screen.
    let _ = stdout.write_all(
        b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?25h\x1b[?1049l",
    );
    let _ = stdout.flush();
    let _ = crossterm::terminal::disable_raw_mode();
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

fn leave(terminal: &mut DefaultTerminal) {
    restore_modes();
    let _ = terminal.show_cursor();
    ratatui::restore();
    TERMINAL_ACTIVE.store(false, Ordering::Relaxed);
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
            let intent = match &update.raw_input {
                MaybeUndefined::Value(input) => Some(intent_of(input)),
                MaybeUndefined::Null => Some(None),
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
                intent,
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
                intent: None,
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
        SessionUpdate::StateUpdate(state) => vec![Update::State(state)],
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

fn intent_of(input: &Value) -> Option<String> {
    input
        .get("intent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|intent| !intent.is_empty())
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
        AgentMessage, AgentThought, AuthMethod, AuthMethodTerminal, AvailableCommand,
        AvailableCommandsUpdate, ContentBlock, EnvVariable, IdleStateUpdate, RunningStateUpdate,
        SessionConfigOption, SessionConfigSelectGroup, SessionConfigSelectOption, SessionUpdate,
        StateUpdate, TextContent, UpdateSessionNotification, UserMessage,
    };
    use agent_client_protocol::{Channel, ConnectTo};
    use crossterm::event::Event;

    use serde_json::json;

    use super::{
        ActiveSessionRoute, AgentInvocation, ConnectedAuthentication, MAX_ATTACHMENTS, MAX_BURST,
        ModelChoice, OPENROUTER_API_KEY_ENV, ProtocolVersion, QueuedUpdate, accept_queued_update,
        active_config_matches, active_session_config, agent_command_for_launch,
        apply_pending_updates, attachments_from_paste, authentication_required,
        client_capabilities, command, credential_storage_for_launch, current_model_choice,
        detach_from_controlling_terminal, durable_session_id, effort_state, error_detail, handle,
        message_of, osc52, previous_session_for_resume, prompt_blocks, readable,
        refresh_config_state, refresh_session_after_auth, save_effort_default_to,
        save_model_defaults_to, terminal_auth_command, transition_route, translate,
        translate_for_session, usable_terminal_auth_methods, user_message_of,
        wait_for_connected_authentication, wire,
    };
    use crate::{
        tools::mcp::CredentialStorage,
        tui::app::{Action, App, SessionDialog, SessionRename, SubmittedPrompt, Update},
    };

    fn command_args(command: &tokio::process::Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn only_authentication_failures_enter_login_recovery() {
        let methods = [
            AuthMethodTerminal::new("openrouter", "OpenRouter").args(vec![
                "auth".into(),
                "login".into(),
                "openrouter".into(),
            ]),
        ];
        let required = agent_client_protocol::Error::auth_required().data(
            crate::protocols::acp::AuthenticationRequiredData::new(
                "openrouter",
                "run `kit auth login openrouter` before using OpenRouter",
            )
            .into_value(),
        );
        assert!(authentication_required(&required, &methods));
        assert!(authentication_required(
            &agent_client_protocol::Error::auth_required(),
            &methods,
        ));
        assert!(!authentication_required(
            &agent_client_protocol::Error::internal_error().data(required.data.clone()),
            &methods,
        ));
        assert_eq!(
            error_detail(&required),
            "run `kit auth login openrouter` before using OpenRouter"
        );
        assert!(!authentication_required(
            &agent_client_protocol::util::internal_error(
                "stored OpenRouter credentials cannot be used with a noncanonical endpoint",
            ),
            &methods,
        ));
        assert!(!authentication_required(
            &agent_client_protocol::Error::auth_required().data(
                crate::protocols::acp::AuthenticationRequiredData::new(
                    "unadvertised",
                    "authentication required",
                )
                .into_value(),
            ),
            &methods,
        ));
    }

    #[test]
    fn relative_credential_storage_is_resolved_for_all_child_processes() {
        assert!(matches!(
            credential_storage_for_launch(
                &CredentialStorage::Filesystem(PathBuf::from("credentials")),
                || Ok(PathBuf::from("/caller")),
            )
            .unwrap(),
            CredentialStorage::Filesystem(path) if path == std::path::Path::new("/caller/credentials")
        ));
        assert!(matches!(
            credential_storage_for_launch(&CredentialStorage::Keychain, || {
                panic!("absolute storage must not query the current directory")
            })
            .unwrap(),
            CredentialStorage::Keychain
        ));
    }

    #[test]
    fn terminal_auth_is_negotiated_and_only_usable_methods_are_exposed() {
        assert!(
            client_capabilities(&CredentialStorage::Keychain)
                .auth
                .and_then(|auth| auth.terminal)
                .is_some()
        );

        assert!(
            client_capabilities(&CredentialStorage::Memory)
                .auth
                .is_none()
        );

        let methods = vec![
            AuthMethod::Terminal(AuthMethodTerminal::new("empty", "Empty")),
            AuthMethod::Terminal(AuthMethodTerminal::new("openai", "ChatGPT").args(vec![
                "auth".into(),
                "login".into(),
                "openai".into(),
            ])),
        ];
        let usable = usable_terminal_auth_methods(&methods);
        assert_eq!(usable.len(), 2);
        assert_eq!(usable[0].method_id.0.as_ref(), "empty");
        assert_eq!(usable[1].method_id.0.as_ref(), "openai");
    }

    #[test]
    fn agent_launch_tracks_transitioned_session_and_preserves_base_options() {
        let root = tempfile::tempdir().unwrap();
        let mcp_config = root.path().join("mcp.toml");
        let telemetry = crate::telemetry::Settings::try_new(None, false, 12, 4096).unwrap();
        let credentials = CredentialStorage::Memory;
        let resumed = agent_command_for_launch(
            root.path(),
            "test",
            crate::ProviderKind::Speakeasy,
            None,
            None,
            Some("127.0.0.1:0"),
            Some(&mcp_config),
            &telemetry,
            &credentials,
            "B",
            true,
            false,
        )
        .unwrap();
        let resumed_args = command_args(&resumed);
        assert!(
            resumed_args
                .windows(2)
                .any(|args| args == ["--session-id", "B"])
        );
        assert!(resumed_args.iter().any(|arg| arg == "--resume"));
        assert!(!resumed_args.iter().any(|arg| arg == "A"));

        let invocation = AgentInvocation::from_command(resumed.as_std());
        let method = AuthMethodTerminal::new("openai", "ChatGPT")
            .args(vec!["--terminal-auth-login".into(), "openai".into()]);
        let terminal_auth = terminal_auth_command(&invocation, root.path(), &method);
        let terminal_auth_args = command_args(&terminal_auth);
        assert!(
            terminal_auth_args
                .windows(2)
                .any(|args| args == ["--session-id", "B"])
        );
        assert!(terminal_auth_args.iter().any(|arg| arg == "--resume"));
        assert!(!terminal_auth_args.iter().any(|arg| arg == "A"));

        let new_session = agent_command_for_launch(
            root.path(),
            "test",
            crate::ProviderKind::Speakeasy,
            None,
            None,
            Some("127.0.0.1:0"),
            Some(&mcp_config),
            &telemetry,
            &credentials,
            "C",
            false,
            false,
        )
        .unwrap();
        let new_args = command_args(&new_session);
        assert!(
            new_args
                .windows(2)
                .any(|args| args == ["--session-id", "C"])
        );
        assert!(!new_args.iter().any(|arg| arg == "--resume"));
        assert!(!new_args.iter().any(|arg| matches!(arg.as_str(), "A" | "B")));
        for args in [&resumed_args, &new_args] {
            assert!(args.windows(2).any(|args| args == ["--a2a", "127.0.0.1:0"]));
            assert!(args.windows(2).any(|args| {
                args[0] == "--mcp-config" && args[1] == mcp_config.to_string_lossy()
            }));
        }
    }

    #[test]
    fn terminal_auth_preserves_base_invocation_for_reconnect() {
        let root = tempfile::tempdir().unwrap();
        let base_root = tempfile::tempdir().unwrap();
        let credentials = root.path().join("credentials");
        let mut base = tokio::process::Command::new("kit-agent");
        base.args(["serve", "--model", "test-model"]);
        base.current_dir(base_root.path());
        CredentialStorage::Filesystem(credentials.clone()).append_cli_args(&mut base);
        base.env("KIT_BASE_TEST", "base");
        base.env(OPENROUTER_API_KEY_ENV, "secret");
        let invocation = AgentInvocation::from_command(base.as_std());
        assert!(
            invocation
                .env
                .iter()
                .all(|(name, _)| name != OPENROUTER_API_KEY_ENV)
        );
        let method = AuthMethodTerminal::new("openai", "ChatGPT")
            .args(vec!["--terminal-auth-login".into(), "openai".into()])
            .env(vec![EnvVariable::new("KIT_AUTH_TEST", "set")]);
        let command = terminal_auth_command(&invocation, root.path(), &method);
        assert_eq!(
            command.as_std().get_program(),
            std::ffi::OsStr::new("kit-agent")
        );
        assert_eq!(command.as_std().get_current_dir(), Some(root.path()));
        assert_eq!(
            command_args(&command),
            [
                "serve",
                "--model",
                "test-model",
                "--credential-store",
                "file",
                "--credential-dir",
                credentials.to_str().unwrap(),
                "--terminal-auth-login",
                "openai",
            ]
        );
        assert!(command.as_std().get_envs().any(|(name, value)| {
            name == "KIT_BASE_TEST" && value == Some(std::ffi::OsStr::new("base"))
        }));
        assert!(command.as_std().get_envs().any(|(name, value)| {
            name == "KIT_AUTH_TEST" && value == Some(std::ffi::OsStr::new("set"))
        }));
        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(name, value)| { name == OPENROUTER_API_KEY_ENV && value.is_none() })
        );

        let replacement = invocation.command();
        assert_eq!(
            replacement.as_std().get_program(),
            std::ffi::OsStr::new("kit-agent")
        );
        assert_eq!(
            replacement.as_std().get_current_dir(),
            Some(base_root.path())
        );
        assert_eq!(
            command_args(&replacement),
            [
                "serve",
                "--model",
                "test-model",
                "--credential-store",
                "file",
                "--credential-dir",
                credentials.to_str().unwrap(),
            ]
        );
        assert!(replacement.as_std().get_envs().any(|(name, value)| {
            name == "KIT_BASE_TEST" && value == Some(std::ffi::OsStr::new("base"))
        }));
        assert!(
            replacement
                .as_std()
                .get_envs()
                .all(|(name, _)| name != "KIT_AUTH_TEST")
        );
    }

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
    fn model_switch_request_rejects_invalid_actions_and_missing_warnings() {
        use crate::protocols::acp::model_switch::{Confirmation, Decision, META, Warning};
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openrouter".into(),
            "original".into(),
            String::new(),
        );
        let session = wire::SessionId::new("first");
        assert!(
            super::prepare_model_switch_request(&mut app, Action::None, session.clone()).is_err()
        );
        assert!(
            super::prepare_model_switch_request(
                &mut app,
                Action::ConfirmModelSwitch(Decision::Continue),
                session.clone()
            )
            .is_err()
        );
        assert!(app.model_switch.is_none());
        let choice = ModelChoice {
            id: "openrouter:target".into(),
            provider: "openrouter".into(),
            model: "target".into(),
        };
        let (operation, request) = super::prepare_model_switch_request(
            &mut app,
            Action::SelectModel {
                choice,
                save_defaults: true,
            },
            session.clone(),
        )
        .unwrap();
        assert!(request.meta.is_none());
        assert!(
            super::prepare_model_switch_request(
                &mut app,
                Action::ConfirmModelSwitch(Decision::Compact),
                session.clone()
            )
            .is_err()
        );
        assert_eq!(app.model_switch.as_ref().unwrap().id, operation);
        for decision in [Decision::Continue, Decision::Compact] {
            app.model_switch.as_mut().unwrap().warning = Some(Warning {
                token: 42,
                guarded_tokens: "120000".into(),
                target_window: 150000,
            });
            let (confirmed_operation, request) = super::prepare_model_switch_request(
                &mut app,
                Action::ConfirmModelSwitch(decision),
                session.clone(),
            )
            .unwrap();
            let confirmation: Confirmation =
                serde_json::from_value(request.meta.unwrap()[META].clone()).unwrap();
            assert_eq!(confirmation.token, 42);
            assert_eq!(confirmation.action, decision);
            assert_eq!(confirmed_operation, operation);
            let pending = app.model_switch.as_ref().unwrap();
            assert!(pending.warning.is_none());
            assert!(pending.save_defaults);
            assert_eq!(pending.choice.id, "openrouter:target");
            assert_eq!(app.model, "original");
        }
    }

    #[test]
    fn model_switch_completion_requires_current_session_and_operation() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openrouter".into(),
            "original".into(),
            String::new(),
        );
        let route = Arc::new(Mutex::new(ActiveSessionRoute {
            id: "first".into(),
            generation: 0,
        }));
        let choice = ModelChoice {
            id: "openrouter:target".into(),
            provider: "openrouter".into(),
            model: "target".into(),
        };
        let operation = app.begin_model_switch(choice, false).unwrap();
        assert!(
            super::take_model_switch_completion(&mut app, &route, 0, operation + 1)
                .unwrap()
                .is_none()
        );
        transition_route(&route, "second".into());
        assert!(
            super::take_model_switch_completion(&mut app, &route, 0, operation)
                .unwrap()
                .is_none()
        );
        assert!(app.model_switch.is_some());
        assert!(
            super::take_model_switch_completion(&mut app, &route, 1, operation)
                .unwrap()
                .is_some()
        );
        assert!(
            super::take_model_switch_completion(&mut app, &route, 1, operation)
                .unwrap()
                .is_none()
        );
        assert_eq!(app.model, "original");
    }

    #[test]
    fn model_switch_completion_reports_poisoned_route_without_discarding_switch() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openrouter".into(),
            "original".into(),
            String::new(),
        );
        let route = Arc::new(Mutex::new(ActiveSessionRoute {
            id: "first".into(),
            generation: 0,
        }));
        let operation = app
            .begin_model_switch(
                ModelChoice {
                    id: "openrouter:target".into(),
                    provider: "openrouter".into(),
                    model: "target".into(),
                },
                false,
            )
            .unwrap();
        let _ = std::panic::catch_unwind(|| {
            let _guard = route.lock().unwrap();
            panic!("poison session route");
        });
        assert!(super::take_model_switch_completion(&mut app, &route, 0, operation).is_err());
        assert_eq!(app.model_switch.as_ref().unwrap().id, operation);
        assert_eq!(app.model, "original");
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
    fn queued_updates_are_applied_in_bounded_bursts() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::new(
            root.path().into(),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        let route = Arc::new(Mutex::new(ActiveSessionRoute {
            id: "session".into(),
            generation: 0,
        }));
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        for index in 0..=MAX_BURST {
            updates_tx
                .send(QueuedUpdate::global(Update::Log(index.to_string())))
                .unwrap();
        }
        let first = updates_rx.try_recv().unwrap();

        apply_pending_updates(&mut app, &route, &mut updates_rx, first);

        assert!(updates_rx.try_recv().is_ok());
        assert!(updates_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn connected_terminal_authentication_prepares_before_observing_agent_exit() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::new(
            root.path().into(),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        let route = Arc::new(Mutex::new(ActiveSessionRoute {
            id: "session".into(),
            generation: 0,
        }));
        let (_updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();
        exit_tx
            .send(Err(std::io::Error::other("agent exited")))
            .unwrap();
        let authentication =
            std::future::pending::<Option<std::io::Result<std::process::ExitStatus>>>();
        let prepared = std::cell::Cell::new(false);

        let result = wait_for_connected_authentication(
            authentication,
            || prepared.set(true),
            &mut app,
            &route,
            &mut updates_rx,
            &mut exit_rx,
        )
        .await;

        assert!(prepared.get());
        assert!(matches!(
            result,
            ConnectedAuthentication::AgentExited(Ok(Err(_)))
        ));
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
            [Update::State(StateUpdate::Running(_))]
        ));
        let idle = UpdateSessionNotification::new(
            "session",
            SessionUpdate::StateUpdate(StateUpdate::Idle(IdleStateUpdate::new())),
        );
        assert!(matches!(
            translate_for_session(idle, "session").as_slice(),
            [Update::State(StateUpdate::Idle(idle))] if idle.stop_reason.is_none()
        ));
    }

    #[test]
    fn preserves_requires_action_and_terminal_state_reasons() {
        let blocked = UpdateSessionNotification::new(
            "session",
            SessionUpdate::StateUpdate(StateUpdate::RequiresAction(
                wire::RequiresActionStateUpdate::new(),
            )),
        );
        assert!(matches!(
            translate_for_session(blocked, "session").as_slice(),
            [Update::State(StateUpdate::RequiresAction(_))]
        ));
        for reason in [
            wire::StopReason::EndTurn,
            wire::StopReason::Cancelled,
            wire::StopReason::Other("custom".into()),
        ] {
            let idle = UpdateSessionNotification::new(
                "session",
                SessionUpdate::StateUpdate(StateUpdate::Idle(
                    IdleStateUpdate::new().stop_reason(reason.clone()),
                )),
            );
            assert!(matches!(
                translate_for_session(idle, "session").as_slice(),
                [Update::State(StateUpdate::Idle(idle))] if idle.stop_reason.as_ref() == Some(&reason)
            ));
        }
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
    fn translates_trimmed_compose_intent() {
        let update = UpdateSessionNotification::new(
            "session",
            SessionUpdate::ToolCallUpdate(
                wire::ToolCallUpdate::new("tool-1")
                    .title("compose")
                    .raw_input(json!({
                        "script": "return 1",
                        "intent": "  Check the project.  "
                    })),
            ),
        );

        assert!(matches!(
            translate_for_session(update, "session").as_slice(),
            [Update::ToolPatched { intent: Some(Some(intent)), script: Some(script), .. }]
                if intent == "Check the project." && script == "return 1"
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

    #[tokio::test]
    async fn queued_mutation_wait_does_not_block_delivery_updates_or_cancel() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        app.apply(Update::SteerAccepted {
            editable: true,
            id: "a".into(),
            text: "pending".into(),
        });
        app.paste("draft");
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        let token = app.begin_steer_mutation("a", None).unwrap();
        let route = Arc::new(Mutex::new(super::ActiveSessionRoute {
            id: "session".into(),
            generation: 1,
        }));
        let (updates, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let (release, waiting) = tokio::sync::oneshot::channel();
        let task = super::spawn_steer_mutation(1, "a".into(), token, updates.clone(), async move {
            waiting.await.unwrap();
            Ok(())
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        updates
            .send(QueuedUpdate::for_session(
                1,
                Update::UserMessage {
                    id: "a".into(),
                    text: "delivered".into(),
                    images: vec![],
                    append: false,
                },
            ))
            .unwrap();
        let first = receiver.recv().await.unwrap();
        apply_pending_updates(&mut app, &route, &mut receiver, first);
        assert!(app.pending_steers.is_empty());
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Cancel
        ));
        assert_eq!(app.editor.text(), "draft");
        assert!(!task.is_finished());
        release.send(()).unwrap();
        task.await.unwrap();
        let completion = receiver.recv().await.unwrap();
        apply_pending_updates(&mut app, &route, &mut receiver, completion);
        assert!(app.pending_steers.is_empty());
        assert_eq!(app.editor.text(), "draft");
    }

    #[tokio::test]
    async fn queued_mutation_completion_is_scoped_to_its_starting_session_generation() {
        let route = Arc::new(Mutex::new(super::ActiveSessionRoute {
            id: "session".into(),
            generation: 1,
        }));
        let (updates, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let (release, waiting) = tokio::sync::oneshot::channel();
        let task = super::spawn_steer_mutation(1, "a".into(), 7, updates, async move {
            waiting.await.unwrap();
            Err(agent_client_protocol::Error::invalid_params()
                .data(json!({"reason": "already_delivered"})))
        });
        super::transition_route(&route, "other-session".into());
        release.send(()).unwrap();
        task.await.unwrap();
        let completion = receiver.recv().await.unwrap();
        assert_eq!(completion.generation, Some(1));
        assert!(
            matches!(&completion.update, Update::SteerMutationFinished { token: 7, result: Err(error), .. } if error.unavailable)
        );
        assert!(accept_queued_update(&route, completion).is_none());
    }

    #[test]
    fn queued_media_editability_uses_actual_submitted_content() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for (name, mime, kind) in [
            ("image.png", "image/png", super::AttachmentKind::Image),
            ("audio.mp3", "audio/mpeg", super::AttachmentKind::Audio),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(name);
            std::fs::write(&path, b"media").unwrap();
            let mut app = App::new(
                directory.path().into(),
                "provider".into(),
                "model".into(),
                "a2a".into(),
            );
            app.can_steer = true;
            app.can_replace_steer = true;
            app.apply(Update::State(StateUpdate::Running(
                RunningStateUpdate::new(),
            )));
            app.attach(path, mime, kind, 5);
            let Action::Submit { prompt, inject } =
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            else {
                panic!("media steer must remain supported");
            };
            assert!(inject);
            let blocks = prompt_blocks(&prompt).unwrap();
            assert_eq!(blocks.len(), 2);
            let editable = super::pending_steer_is_editable(&blocks);
            assert!(!editable);
            app.clear_attachments();
            app.apply(Update::SteerAccepted {
                id: "media".into(),
                text: prompt.text,
                editable,
            });
            app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
            assert!(matches!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Action::None
            ));
            assert!(!app.editing_steer());
            assert!(matches!(
                app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
                Action::RevokeSteer { .. }
            ));
        }
    }

    #[test]
    fn queued_stale_attachment_metadata_does_not_disable_plain_text_editing() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.can_steer = true;
        app.can_replace_steer = true;
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        app.attach(
            PathBuf::from("nonexistent.png"),
            "image/png",
            super::AttachmentKind::Image,
            5,
        );
        app.editor.clear(); // Remove the attachment placeholder, leaving stale metadata.
        app.paste("plain steer");
        assert_eq!(app.attachments.len(), 1);
        let Action::Submit { prompt, inject } =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected plain steer");
        };
        assert!(inject);
        assert!(prompt.attachments.is_empty());
        let blocks = prompt_blocks(&prompt).unwrap();
        let editable = super::pending_steer_is_editable(&blocks);
        assert!(editable);
        app.clear_attachments();
        app.apply(Update::SteerAccepted {
            id: "plain".into(),
            text: prompt.text,
            editable,
        });
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.editing_steer());
    }

    #[test]
    fn queued_replace_capability_requires_explicit_true() {
        use agent_client_protocol::schema::v2::{
            SessionInjectCapabilities, SessionInjectPendingCapabilities,
        };
        assert!(!super::supports_pending_replace(None));
        for pending in [
            None,
            Some(SessionInjectPendingCapabilities::new()),
            Some(SessionInjectPendingCapabilities::new().replace(false)),
        ] {
            let inject = SessionInjectCapabilities::new(vec![]).pending(pending);
            assert!(!super::supports_pending_replace(Some(&inject)));
        }
        let inject = SessionInjectCapabilities::new(vec![])
            .pending(SessionInjectPendingCapabilities::new().replace(true));
        assert!(super::supports_pending_replace(Some(&inject)));
    }

    #[test]
    fn queued_mutation_errors_retire_only_known_missing_or_delivered_ids() {
        for reason in [
            "already_delivered",
            "unknown_message_id",
            "replace_not_supported",
            "temporary_failure",
        ] {
            let error = agent_client_protocol::Error::invalid_params()
                .data(json!({"reason": reason, "messageId": "a"}));
            assert_eq!(
                super::pending_message_unavailable(&error),
                matches!(reason, "already_delivered" | "unknown_message_id")
            );
        }
        assert!(!super::pending_message_unavailable(
            &agent_client_protocol::Error::invalid_params()
        ));
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
    fn queued_edit_media_paste_remains_text_and_preserves_original_attachments() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"png").unwrap();
        let mut app = App::new(
            directory.path().into(),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.can_replace_steer = true;
        app.apply(Update::SteerAccepted {
            editable: true,
            id: "a".into(),
            text: "pending".into(),
        });
        handle(&mut app, Event::Paste(path.display().to_string()));
        let draft = app.editor.text().to_owned();
        let attachments = app.attachments.clone();
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        handle(&mut app, Event::Paste("ignored while selecting".into()));
        assert_eq!(app.editor.text(), draft);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle(&mut app, Event::Paste(path.display().to_string()));
        assert!(app.attachments.is_empty());
        assert!(app.editor.text().contains(path.to_str().unwrap()));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.attachments, attachments);
        assert_eq!(app.editor.text(), draft);
    }

    #[test]
    fn async_session_catalog_rename_paste_takes_precedence_over_queue_focus() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"png").unwrap();
        for populated in [false, true] {
            let mut app = App::new(
                directory.path().into(),
                "provider".into(),
                "model".into(),
                "a2a".into(),
            );
            app.paste("/sessions");
            assert!(matches!(
                handle(
                    &mut app,
                    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                ),
                Action::ListSessions
            ));
            app.paste("parked draft");
            if populated {
                app.apply(Update::SteerAccepted {
                    id: "pending".into(),
                    text: "queued text".into(),
                    editable: true,
                });
            }
            handle(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
            );
            assert_eq!(app.queue_focused, populated);
            app.apply(Update::SessionCatalog(Ok(vec![
                crate::session::CatalogEntry {
                    id: "saved".into(),
                    title: Some("Saved".into()),
                    preview: None,
                    is_subagent: false,
                    updated_at: 0,
                },
            ])));
            handle(
                &mut app,
                Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            );
            assert!(app.session_rename_active());
            handle(&mut app, Event::Paste(path.display().to_string()));
            assert!(matches!(
                app.session_dialog.as_ref().unwrap().rename.as_ref(),
                Some(SessionRename::Editing(input)) if input == path.to_str().unwrap()
            ));
            assert!(app.attachments.is_empty());
            assert_eq!(app.queue_focused, populated);
            assert_eq!(app.editor.text(), "parked draft");
        }
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
    fn successful_login_refresh_preserves_active_provider_model_and_effort() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "openrouter:active-model",
                vec![SessionConfigSelectGroup::new(
                    "openrouter",
                    "OpenRouter",
                    vec![
                        SessionConfigSelectOption::new("openrouter:old-model", "old-model"),
                        SessionConfigSelectOption::new("openrouter:active-model", "active-model"),
                    ],
                )],
            ),
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning effort",
                "high",
                vec![SessionConfigSelectGroup::new(
                    "reasoning-effort",
                    "Reasoning effort",
                    vec![
                        SessionConfigSelectOption::new("default", "Default"),
                        SessionConfigSelectOption::new("high", "High"),
                    ],
                )],
            ),
        ];
        let mut app = App::new(
            PathBuf::from("."),
            "openrouter".into(),
            "active-model".into(),
            "127.0.0.1:4321".into(),
        );
        app.set_model_choices(super::model_choices(Some(&options)));
        app.set_effort(
            "high".into(),
            vec![
                crate::tui::app::EffortChoice {
                    id: "default".into(),
                    name: "Default".into(),
                },
                crate::tui::app::EffortChoice {
                    id: "high".into(),
                    name: "High".into(),
                },
            ],
        );

        let active = active_session_config(&app);
        assert_eq!(active.model, "openrouter:active-model");
        assert_eq!(active.reasoning_effort.as_deref(), Some("high"));
        assert!(active_config_matches(&active, &options));
    }

    #[test]
    fn successful_login_refresh_preserves_a_custom_model_outside_the_catalog() {
        let mut app = App::new(
            PathBuf::from("."),
            "openai-subscription".into(),
            "custom-model".into(),
            "a2a".into(),
        );
        app.set_model_choices(vec![ModelChoice {
            id: "openai-subscription:gpt-5.4".into(),
            provider: "openai-subscription".into(),
            model: "gpt-5.4".into(),
        }]);

        assert_eq!(
            active_session_config(&app).model,
            "openai-subscription:custom-model"
        );
    }

    #[tokio::test]
    async fn successful_login_refresh_is_scoped_to_the_active_session() {
        let mut app = App::new(
            PathBuf::from("."),
            "openrouter".into(),
            "same-model".into(),
            "127.0.0.1:4321".into(),
        );
        let options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "openrouter:same-model",
            vec![SessionConfigSelectGroup::new(
                "openrouter",
                "OpenRouter",
                vec![SessionConfigSelectOption::new(
                    "openrouter:same-model",
                    "same-model",
                )],
            )],
        )];
        app.set_model_choices(super::model_choices(Some(&options)));
        let active = active_session_config(&app);

        let root = tempfile::tempdir().unwrap();
        let credentials = crate::credentials::CredentialStorage::Memory;
        crate::provider::store_openrouter_test_credentials(&credentials);
        let runtime = crate::runtime::Runtime::new_with_provider_credentials_and_effort(
            root.path(),
            "same-model",
            crate::ProviderKind::OpenRouter,
            credentials,
            None,
        )
        .unwrap();
        let registry = crate::protocols::acp::SessionRegistry::new();
        let agent = crate::protocols::acp::v2::component(runtime, registry).unwrap();
        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn(async move { agent.connect_to(agent_transport).await });
        let workspace = root.path().to_path_buf();

        agent_client_protocol::Client
            .v2()
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(wire::InitializeRequest::new(
                        ProtocolVersion::V2,
                        wire::Implementation::new("test", "0"),
                    ))
                    .block_task()
                    .await?;
                let active_session = connection
                    .send_request(wire::NewSessionRequest::new(workspace.clone()))
                    .block_task()
                    .await?;
                let unrelated_session = connection
                    .send_request(wire::NewSessionRequest::new(workspace.clone()))
                    .block_task()
                    .await?;

                refresh_session_after_auth(&connection, active_session.session_id.clone(), &active)
                    .await?;

                // This request proves that refreshing the TUI route left another
                // route in the shared serve runtime active.
                connection
                    .send_request(wire::SetSessionConfigOptionRequest::new(
                        unrelated_session.session_id.clone(),
                        "reasoning_effort",
                        "high",
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(wire::CloseSessionRequest::new(active_session.session_id))
                    .block_task()
                    .await?;
                connection
                    .send_request(wire::CloseSessionRequest::new(unrelated_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        server.abort();
        let _ = server.await;
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
        let settings = crate::telemetry::Settings::try_new_with_protocol(
            Some("http://collector:4318".into()),
            crate::telemetry::Protocol::HttpProtobuf,
            false,
            12,
            4096,
        )
        .unwrap();
        let mut command = tokio::process::Command::new("kit");
        settings.append_cli_args(&mut command);
        let args = command_args(&command);
        assert_eq!(
            args,
            [
                "--otel-endpoint",
                "http://collector:4318/v1/traces",
                "--otel-protocol",
                "http/protobuf",
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

    use super::{RequestInterrupt, Stop, bounded_agent_request, bounded_graceful_close};

    #[tokio::test(start_paused = true)]
    async fn a_stuck_session_request_is_bounded() {
        let (_exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();

        let result = bounded_agent_request(
            future::pending::<()>(),
            &mut exit_rx,
            future::pending(),
            Duration::from_secs(30),
        )
        .await;

        assert!(matches!(result, Err(RequestInterrupt::TimedOut)));
    }

    #[tokio::test]
    async fn agent_exit_cancels_a_session_request() {
        let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();
        exit_tx
            .send(Err(std::io::Error::other("agent exited")))
            .unwrap();

        let result = bounded_agent_request(
            future::pending::<()>(),
            &mut exit_rx,
            future::pending(),
            Duration::from_secs(30),
        )
        .await;

        assert!(matches!(result, Err(RequestInterrupt::AgentExited(None))));
    }

    #[tokio::test]
    async fn stop_cancels_a_session_request() {
        let (_exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();

        let result = bounded_agent_request(
            future::pending::<()>(),
            &mut exit_rx,
            future::ready(()),
            Duration::from_secs(30),
        )
        .await;

        assert!(matches!(result, Err(RequestInterrupt::Stopped)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn storage_exit_waits_for_final_flush_and_preserves_failure_status() {
        use tokio::io::AsyncReadExt;
        for code in [0, 1] {
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "cat >/dev/null; sleep 0.05; printf recovered; exit {code}"
                ))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut stdout = child.stdout.take().unwrap();
            // Like dropping the ACP transport: EOF initiates child shutdown.
            drop(child.stdin.take());
            let status = super::wait_for_storage_exit(&mut child, Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(status.code(), Some(code));
            let mut flushed = String::new();
            stdout.read_to_string(&mut flushed).await.unwrap();
            assert_eq!(flushed, "recovered");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn storage_exit_timeout_reports_possible_loss_and_reaps_child() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let error = super::wait_for_storage_exit(&mut child, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("unpersisted data may be lost"));
        assert!(child.try_wait().unwrap().is_some());
    }

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
