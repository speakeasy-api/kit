//! Generic parent-owned ACP subprocesses used for nested agents.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

use agent_client_protocol::{ByteStreams, schema::ProtocolVersion};
use agentkit_acp::{
    CancelNotification, CloseSessionRequest, ContentBlock, ForkSessionRequest, PermissionOption,
    PermissionOptionKind, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigKind, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, StopReason,
};
use agentkit_core::TurnCancellation;
use futures_util::future::{Either, select};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::tools::mcp::CredentialStorage;

const HANDSHAKE: Duration = Duration::from_secs(30);
const PRE_HANDSHAKE_EXIT_SETTLE: Duration = Duration::from_millis(250);
const CANCEL_SETTLE: Duration = Duration::from_secs(5);
const MAX_CAPTURED_UPDATES: usize = 64;
const MAX_CAPTURED_UPDATE_BYTES: usize = 64 * 1024;
const FORK_PARENT_ID_META: &str = "kit.subagent.parent_id";
const FORK_PARENT_NAME_META: &str = "kit.subagent.parent_name";
pub const BUILTIN_HARNESS: &str = "acp.kit";

/// How a headless nested ACP client handles permission requests.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcpPermissionPolicy {
    /// Select a rejection option when offered, otherwise cancel the request.
    #[default]
    Deny,
    /// Always cancel the request without selecting an option.
    Cancel,
}

/// A trusted argv-only ACP harness profile from `~/.kit/config.toml`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AcpHarnessProfile {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub permissions: AcpPermissionPolicy,
}

/// Model aliases and explicit-override policy for one subagent harness.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct SubagentHarnessPolicy {
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    pub allow_model_overrides: Option<Vec<String>>,
}

/// Validated named ACP harness profiles. `acp.kit` is always Kit; its launch base may be overridden.
#[derive(Clone, Debug, Default)]
pub struct AcpHarnesses {
    profiles: Arc<BTreeMap<String, AcpHarnessProfile>>,
    model_policies: Arc<BTreeMap<String, SubagentHarnessPolicy>>,
}

impl AcpHarnesses {
    pub fn new(profiles: BTreeMap<String, AcpHarnessProfile>) -> Result<Self, String> {
        for (name, profile) in &profiles {
            if name.trim().is_empty() || name.contains(char::is_whitespace) || name.contains('.') {
                return Err(
                    "ACP harness names must be non-empty and contain neither whitespace nor dots"
                        .into(),
                );
            }
            if profile.command.trim().is_empty() {
                return Err(format!("ACP harness {name:?} has an empty command"));
            }
        }
        Ok(Self {
            profiles: Arc::new(profiles),
            model_policies: Arc::default(),
        })
    }

    pub fn contains(&self, reference: &str) -> bool {
        self.profile_name(reference)
            .is_some_and(|name| name == "kit" || self.profiles.contains_key(name))
    }

    pub fn is_kit(&self, reference: &str) -> bool {
        reference == BUILTIN_HARNESS
    }

    pub fn references(&self) -> Vec<String> {
        std::iter::once(BUILTIN_HARNESS.to_string())
            .chain(
                self.profiles
                    .keys()
                    .filter(|name| name.as_str() != "kit")
                    .map(|name| format!("acp.{name}")),
            )
            .collect()
    }

    pub fn with_model_policies(
        mut self,
        policies: BTreeMap<String, SubagentHarnessPolicy>,
    ) -> Result<Self, String> {
        for (harness, policy) in &policies {
            if !self.contains(harness) {
                return Err(format!("unknown subagent model policy harness {harness:?}"));
            }
            for (alias, model) in &policy.models {
                if alias.trim().is_empty() {
                    return Err(format!(
                        "subagent model aliases for {harness:?} must be non-empty"
                    ));
                }
                if model.trim().is_empty() {
                    return Err(format!(
                        "subagent model alias {alias:?} for {harness:?} has an empty value"
                    ));
                }
                if let Some(allowed) = &policy.allow_model_overrides
                    && !allowed.contains(model)
                {
                    return Err(format!(
                        "subagent model alias {alias:?} resolves to {model:?}, which is not in allow_model_overrides for {harness:?}"
                    ));
                }
            }
            if policy
                .allow_model_overrides
                .as_ref()
                .is_some_and(|allowed| allowed.iter().any(|model| model.trim().is_empty()))
            {
                return Err(format!(
                    "allow_model_overrides for {harness:?} must not contain empty values"
                ));
            }
        }
        self.model_policies = Arc::new(policies);
        Ok(self)
    }

    pub(crate) fn resolve_model(&self, harness: &str, requested: &str) -> Result<String, String> {
        let Some(policy) = self.model_policies.get(harness) else {
            return Ok(requested.to_owned());
        };
        let resolved = policy
            .models
            .get(requested)
            .map_or(requested, String::as_str);
        if let Some(allowed) = &policy.allow_model_overrides
            && !allowed.iter().any(|model| model == resolved)
        {
            return Err(format!(
                "model override {requested:?} resolves to {resolved:?}, which is not allowed for ACP harness {harness:?}"
            ));
        }
        Ok(resolved.to_owned())
    }

    fn profile_name<'a>(&self, reference: &'a str) -> Option<&'a str> {
        let name = reference.strip_prefix("acp.")?;
        (!name.is_empty() && !name.contains('.')).then_some(name)
    }

    fn launch_context(&self, reference: &str) -> LaunchContext {
        let source = if reference == BUILTIN_HARNESS && !self.profiles.contains_key("kit") {
            "built-in current executable"
        } else if reference == BUILTIN_HARNESS {
            "configured acp.kit profile"
        } else {
            "configured ACP profile"
        };
        LaunchContext {
            harness: reference.into(),
            source,
        }
    }

    fn permission_policy(&self, reference: &str) -> Result<AcpPermissionPolicy, String> {
        let name = self.profile_name(reference).ok_or_else(|| {
            format!("ACP harness references must use acp.<name>, got {reference:?}")
        })?;
        if name == "kit" {
            Ok(self
                .profiles
                .get(name)
                .map_or(AcpPermissionPolicy::Deny, |profile| profile.permissions))
        } else {
            self.profiles
                .get(name)
                .map(|profile| profile.permissions)
                .ok_or_else(|| format!("unknown ACP harness {reference:?}"))
        }
    }

    fn spawn(
        &self,
        reference: &str,
        config: &ChildConfig,
        persisted: Option<(&str, bool)>,
        depth: usize,
    ) -> Result<Command, String> {
        let name = self.profile_name(reference).ok_or_else(|| {
            format!("ACP harness references must use acp.<name>, got {reference:?}")
        })?;
        let mut command = if self.is_kit(reference) {
            self.kit_command(config, persisted, depth)?
        } else {
            let profile = self
                .profiles
                .get(name)
                .ok_or_else(|| format!("unknown ACP harness {reference:?}"))?;
            let mut command = Command::new(&profile.command);
            command.args(&profile.args);
            command.env_remove("OPENROUTER_API_KEY");
            command
        };
        // Every trusted profile is spawned directly (never through a shell)
        // with the configured subagent root as cwd.
        command.current_dir(&config.root);
        Ok(command)
    }

    /// Builds only the Kit profile, appending invariants after either the
    /// configured executable/base argv or the current-executable default.
    fn kit_command(
        &self,
        config: &ChildConfig,
        persisted: Option<(&str, bool)>,
        depth: usize,
    ) -> Result<Command, String> {
        let mut command = if let Some(profile) = self.profiles.get("kit") {
            let mut command = Command::new(&profile.command);
            command.args(&profile.args);
            command
        } else {
            let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            command.arg("acp");
            command
        };
        let (id, resume) = persisted.ok_or("Kit harness requires a persistent session")?;
        command
            .arg("--root")
            .arg(&config.root)
            .arg("--model")
            .arg(&config.model)
            .arg("--provider")
            .arg(config.provider.as_str())
            .arg("--reasoning-effort")
            .arg(
                config
                    .reasoning_effort
                    .map_or("default", crate::ReasoningEffort::as_str),
            )
            .arg("--session-id")
            .arg(id)
            .arg("--subagent-depth")
            .arg(depth.to_string());
        if let (Some(parent_id), Some(parent_name)) = (&config.parent_id, &config.parent_name) {
            command
                .arg("--subagent-parent-id")
                .arg(parent_id)
                .arg(format!("--subagent-parent-name={parent_name}"));
        }
        if resume {
            command.arg("--resume");
        }
        if config.legacy_mcp_config {
            command.arg("--internal-mcp-legacy");
        } else if let Some(path) = &config.configured_mcp_config {
            command.arg("--internal-mcp-config").arg(path);
        } else if config.configured_mcp_config_inherited {
            command.arg("--internal-no-mcp-config");
        }
        if let Some(path) = &config.mcp_config {
            command.arg("--mcp-config").arg(path);
        }
        config.credential_storage.append_cli_args(&mut command);
        config.telemetry.append_cli_args(&mut command);
        if let Some(api_key) = &config.openrouter_api_key {
            command.env("OPENROUTER_API_KEY", api_key.as_str());
        }
        Ok(command)
    }
}

#[derive(Clone, Debug)]
struct LaunchContext {
    harness: String,
    source: &'static str,
}

impl LaunchContext {
    fn error(&self, phase: &str, error: impl std::fmt::Display) -> String {
        format!(
            "ACP harness {phase}: {error} (harness={:?}, source={}, cwd=configured working directory)",
            self.harness, self.source
        )
    }
}

/// The combined `kit serve` command used by the TUI.
pub(crate) fn serve_command(
    root: &Path,
    model: &str,
    provider: crate::ProviderKind,
    reasoning_effort: Option<crate::ReasoningEffort>,
    openrouter_api_key: Option<&crate::provider::OpenRouterApiKey>,
    session_id: &str,
    resume: bool,
) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg("--stdio-protocol-version")
        .arg("2")
        .arg("--root")
        .arg(root)
        .arg("--model")
        .arg(model)
        .arg("--provider")
        .arg(provider.as_str())
        .arg("--reasoning-effort")
        .arg(reasoning_effort.map_or("default", crate::ReasoningEffort::as_str))
        .arg("--session-id")
        .arg(session_id);
    if resume {
        command.arg("--resume");
    }
    if let Some(api_key) = openrouter_api_key {
        command.env("OPENROUTER_API_KEY", api_key.as_str());
    }
    Ok(command)
}

#[derive(Clone)]
pub(crate) struct ChildConfig {
    pub root: PathBuf,
    pub model: String,
    pub provider: crate::ProviderKind,
    pub reasoning_effort: Option<crate::ReasoningEffort>,
    pub openrouter_api_key: Option<crate::provider::OpenRouterApiKey>,
    pub configured_mcp_config: Option<PathBuf>,
    pub configured_mcp_config_inherited: bool,
    pub legacy_mcp_config: bool,
    pub mcp_config: Option<PathBuf>,
    pub credential_storage: CredentialStorage,
    pub telemetry: crate::telemetry::Settings,
    pub harnesses: AcpHarnesses,
    pub default_harness: String,
    /// Immediate owning Kit subagent, present only inside a nested Kit runtime.
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
}

impl ChildConfig {
    pub(crate) fn with_root(mut self, root: PathBuf) -> Self {
        let previous_root = self.root.clone();
        if let Some(path) = &mut self.mcp_config
            && path.is_relative()
        {
            *path = previous_root.join(&*path);
        }
        if let CredentialStorage::Filesystem(path) = &mut self.credential_storage
            && path.is_relative()
        {
            *path = previous_root.join(&*path);
        }
        self.root = root;
        self
    }

    pub(crate) fn with_parent_context(mut self, id: String, name: String) -> Self {
        self.parent_id = Some(id);
        self.parent_name = Some(name);
        self
    }
}

#[derive(Debug)]
pub(crate) enum ChildError {
    Cancelled,
    Failed(String),
    TerminalCancelled,
    TerminalFailed(String),
}
impl std::fmt::Display for ChildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled | Self::TerminalCancelled => f.write_str("nested agent cancelled"),
            Self::Failed(e) | Self::TerminalFailed(e) => f.write_str(e),
        }
    }
}

struct Prompt {
    // The worker, not the waiting caller, owns serialization until settlement.
    serial: tokio::sync::OwnedMutexGuard<()>,
    session_id: SessionId,
    text: String,
    cancellation: TurnCancellation,
    reply: oneshot::Sender<Result<ChildOutput, ChildError>>,
}
struct Fork {
    serial: tokio::sync::OwnedMutexGuard<()>,
    session_id: SessionId,
    model: Option<String>,
    parent: Option<(String, String)>,
    cancellation: TurnCancellation,
    reply: oneshot::Sender<Result<SessionId, ChildError>>,
}
struct Close {
    session_id: SessionId,
    reply: oneshot::Sender<Result<(), ChildError>>,
}
enum Request {
    Prompt(Prompt),
    Fork(Fork),
    Close(Close),
}
enum ActorEvent {
    Request(Option<Request>),
    Fatal,
    TaskReaped,
}

async fn next_actor_event(
    rx: &mut mpsc::Receiver<Request>,
    fatal_rx: &mut mpsc::UnboundedReceiver<()>,
    tasks: &mut JoinSet<()>,
    next_branch: &mut usize,
) -> ActorEvent {
    // Rotate after each winner rather than randomizing ties. A source returning
    // Ready continuously gets a turn within three selections, even under request
    // or completion floods (subject to Tokio's cooperative budget). Keep the
    // cursor across actor iterations. Persistent merged streams would hold the
    // task-set borrow across spawning handlers.
    let mut fatal_open = true;
    std::future::poll_fn(|cx| {
        for offset in 0..3 {
            let branch = (*next_branch + offset) % 3;
            let event = match branch {
                0 => rx.poll_recv(cx).map(ActorEvent::Request),
                1 if fatal_open => match fatal_rx.poll_recv(cx) {
                    Poll::Ready(Some(())) => Poll::Ready(ActorEvent::Fatal),
                    // Closure disables this source for this wait; it is not a
                    // fatal message, nor a completed future to poll again.
                    Poll::Ready(None) => {
                        fatal_open = false;
                        Poll::Pending
                    }
                    Poll::Pending => Poll::Pending,
                },
                2 if !tasks.is_empty() => match tasks.poll_join_next(cx) {
                    Poll::Ready(Some(_)) => Poll::Ready(ActorEvent::TaskReaped),
                    Poll::Ready(None) | Poll::Pending => Poll::Pending,
                },
                _ => Poll::Pending,
            };
            if event.is_ready() {
                *next_branch = (branch + 1) % 3;
                return event;
            }
        }
        // Every live source registered this waker. Empty/closed sources never
        // manufacture a ready event or a self-wake. Direct polling consumes only
        // the winner, with no losing future or receiver borrow left in handlers.
        Poll::Pending
    })
    .await
}

struct Ready {
    session_id: SessionId,
    capabilities: agentkit_acp::AgentCapabilities,
    descendant_parent: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ChildOutput {
    pub text: String,
    pub updates: Vec<Value>,
    pub updates_truncated: bool,
    update_bytes: usize,
}

impl ChildOutput {
    fn record(&mut self, update: SessionUpdate) {
        if let SessionUpdate::AgentMessageChunk(chunk) = &update
            && let ContentBlock::Text(text) = &chunk.content
        {
            self.text.push_str(&text.text);
            return;
        }
        if !matches!(
            update,
            SessionUpdate::AgentMessageChunk(_)
                | SessionUpdate::ToolCall(_)
                | SessionUpdate::ToolCallUpdate(_)
                | SessionUpdate::Plan(_)
        ) {
            return;
        }
        if self.updates.len() >= MAX_CAPTURED_UPDATES {
            self.updates_truncated = true;
            return;
        }
        let Ok(mut value) = serde_json::to_value(&update) else {
            self.updates_truncated = true;
            return;
        };
        if matches!(update, SessionUpdate::ToolCallUpdate(_)) {
            deduplicate_tool_output(&mut value);
        }
        let Ok(encoded) = serde_json::to_vec(&value) else {
            self.updates_truncated = true;
            return;
        };
        if self.update_bytes + encoded.len() > MAX_CAPTURED_UPDATE_BYTES {
            self.updates_truncated = true;
            return;
        }
        self.update_bytes += encoded.len();
        self.updates.push(value);
    }
}

fn deduplicate_tool_output(update: &mut Value) {
    let Some(object) = update.as_object_mut() else {
        return;
    };
    let Some(raw_output) = object.get("rawOutput") else {
        return;
    };
    let Some(content) = object.get("content") else {
        return;
    };
    if rendered_text_only(content).is_some_and(|text| {
        serde_json::from_str::<Value>(text).is_ok_and(|value| value == *raw_output)
    }) {
        object.remove("content");
    }
}

fn rendered_text_only(value: &Value) -> Option<&str> {
    match value {
        Value::Array(values) if values.len() == 1 => rendered_text_only(&values[0]),
        Value::Object(object)
            if object
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "content")) =>
        {
            rendered_text_only(object.get("content")?)
        }
        Value::Object(object)
            if object
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "text"))
                && object.get("type").is_none_or(|value| value == "text") =>
        {
            object.get("text")?.as_str()
        }
        _ => None,
    }
}

/// A logical ACP session. Multiple forked sessions may share one child process.
#[derive(Clone)]
pub(crate) struct ChildSession {
    tx: mpsc::Sender<Request>,
    session_id: SessionId,
    capabilities: agentkit_acp::AgentCapabilities,
    serial: Arc<tokio::sync::Mutex<()>>,
    closed: watch::Receiver<bool>,
    descendant_parent: Option<String>,
}

impl ChildSession {
    pub async fn start(
        config: ChildConfig,
        harness: String,
        persisted: Option<(String, bool)>,
        model: Option<String>,
        depth: usize,
        cancellation: TurnCancellation,
    ) -> Result<Self, ChildError> {
        let context = config.harnesses.launch_context(&harness);
        let actor_context = context.clone();
        let (tx, mut rx) = mpsc::channel(1);
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = watch::channel(false);
        let actor_tx = tx.clone();
        let mut task = tokio::spawn(async move {
            let result = run(
                RunConfig {
                    config,
                    harness,
                    persisted,
                    model,
                    depth,
                    context: actor_context,
                },
                &mut rx,
                ready_tx,
                closed_tx.clone(),
            )
            .await;
            let _ = closed_tx.send(true);
            result
        });
        // Startup is a one-shot race, not a scheduler: either simultaneous result
        // was valid before. Prefer readiness, then actor exit, over cancellation
        // and timeout; abort and join the actor on cancellation/timeout as before.
        let result = match select(
            select(&mut ready_rx, &mut task),
            select(
                std::pin::pin!(cancellation.cancelled()),
                std::pin::pin!(tokio::time::sleep(HANDSHAKE)),
            ),
        )
        .await
        {
            Either::Left((Either::Left((ready, _)), _)) => match ready {
                Ok(Ok(ready)) => Ok(ready),
                Ok(Err(error)) => Err(ChildError::Failed(error)),
                Err(_) => {
                    return Err(ChildError::Failed(match task.await {
                        Ok(Ok(())) => "nested agent exited during startup".into(),
                        Ok(Err(error)) => error,
                        Err(error) => format!("nested agent startup actor failed: {error}"),
                    }));
                }
            },
            Either::Right((Either::Left(((), _)), _)) => Err(ChildError::Cancelled),
            Either::Right((Either::Right(((), _)), _)) => Err(ChildError::Failed(context.error(
                "handshake timeout",
                format!("no response within {} seconds", HANDSHAKE.as_secs()),
            ))),
            Either::Left((Either::Right((joined, _)), _)) => {
                return Err(ChildError::Failed(match joined {
                    Ok(Ok(())) => "nested agent exited during startup".into(),
                    Ok(Err(e)) => e,
                    Err(e) => format!("nested agent startup actor failed: {e}"),
                }));
            }
        };
        match result {
            Ok(ready) => Ok(Self {
                tx: actor_tx,
                session_id: ready.session_id,
                capabilities: ready.capabilities,
                serial: Arc::new(tokio::sync::Mutex::new(())),
                closed: closed_rx,
                descendant_parent: ready.descendant_parent,
            }),
            Err(error) => {
                task.abort();
                let _ = task.await;
                Err(error)
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed() || *self.closed.borrow()
    }

    pub fn closed_signal(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }

    pub fn supports_native_fork(&self) -> bool {
        self.capabilities.session_capabilities.fork.is_some()
    }

    pub async fn close(&self) -> Result<(), ChildError> {
        if let Some(ancestor_id) = &self.descendant_parent {
            crate::events::emit(&crate::events::RuntimeEvent::SubagentDescendantsRemoved {
                ancestor_id: ancestor_id.clone(),
            });
        }
        if self.capabilities.session_capabilities.close.is_none() {
            return if self.tx.strong_count() == 1 {
                Ok(())
            } else {
                Err(ChildError::Failed(
                    "ACP harness does not support closing one session while sibling sessions share its process".into(),
                ))
            };
        }
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Request::Close(Close {
                session_id: self.session_id.clone(),
                reply,
            }))
            .await
            .map_err(|_| {
                ChildError::TerminalFailed("nested agent process is no longer running".into())
            })?;
        response.await.map_err(|_| {
            ChildError::TerminalFailed(
                "nested agent process exited without a close response".into(),
            )
        })?
    }

    pub async fn fork(
        &self,
        model: Option<&str>,
        parent: Option<(String, String)>,
        cancellation: &TurnCancellation,
    ) -> Result<Self, ChildError> {
        // A one-shot admission race: an available gate may win concurrent
        // cancellation. The request retains cancellation after admission.
        let serial = match select(
            std::pin::pin!(self.serial.clone().lock_owned()),
            std::pin::pin!(cancellation.cancelled()),
        )
        .await
        {
            Either::Left((serial, _)) => serial,
            Either::Right(((), _)) => return Err(ChildError::Cancelled),
        };
        if !self.supports_native_fork() {
            return Err(ChildError::Failed(
                "ACP harness does not support session/fork".into(),
            ));
        }
        let descendant_parent = parent.as_ref().map(|(id, _)| id.clone());
        let (reply, response) = oneshot::channel();
        // Admission transfers the gate to the actor; cancellation while the
        // channel is full instead drops the unsent request and releases it.
        match select(
            std::pin::pin!(self.tx.send(Request::Fork(Fork {
                serial,
                session_id: self.session_id.clone(),
                model: model.map(str::to_owned),
                parent,
                cancellation: cancellation.clone(),
                reply,
            }))),
            std::pin::pin!(cancellation.cancelled()),
        )
        .await
        {
            Either::Left((sent, _)) => sent.map_err(|_| {
                ChildError::TerminalFailed("nested agent process is no longer running".into())
            })?,
            Either::Right(((), _)) => return Err(ChildError::Cancelled),
        }
        let session_id = response.await.map_err(|_| {
            ChildError::TerminalFailed("nested agent process exited without a fork response".into())
        })??;
        Ok(Self {
            tx: self.tx.clone(),
            session_id,
            capabilities: self.capabilities.clone(),
            serial: Arc::new(tokio::sync::Mutex::new(())),
            closed: self.closed.clone(),
            descendant_parent,
        })
    }

    pub async fn prompt(
        &self,
        text: String,
        cancellation: TurnCancellation,
    ) -> Result<ChildOutput, ChildError> {
        // A one-shot admission race: an available gate may win concurrent
        // cancellation. The request retains cancellation after admission.
        let serial = match select(
            std::pin::pin!(self.serial.clone().lock_owned()),
            std::pin::pin!(cancellation.cancelled()),
        )
        .await
        {
            Either::Left((serial, _)) => serial,
            Either::Right(((), _)) => return Err(ChildError::Cancelled),
        };
        let (reply, response) = oneshot::channel();
        let request = Request::Prompt(Prompt {
            serial,
            session_id: self.session_id.clone(),
            text,
            cancellation: cancellation.clone(),
            reply,
        });
        // As with fork, a ready send may win concurrent cancellation. Once
        // sent, only actor settlement releases the request's serialization gate.
        match select(
            std::pin::pin!(self.tx.send(request)),
            std::pin::pin!(cancellation.cancelled()),
        )
        .await
        {
            Either::Left((sent, _)) => sent.map_err(|_| {
                ChildError::TerminalFailed("nested agent process is no longer running".into())
            })?,
            Either::Right(((), _)) => return Err(ChildError::Cancelled),
        }
        response.await.map_err(|_| {
            ChildError::TerminalFailed("nested agent process exited without a response".into())
        })?
    }
}

struct RunConfig {
    config: ChildConfig,
    harness: String,
    persisted: Option<(String, bool)>,
    model: Option<String>,
    depth: usize,
    context: LaunchContext,
}

/// Keeps a nested harness's private runtime events out of the parent runtime.
fn harness_diagnostic(label: &str, line: &str) -> Option<String> {
    if matches!(
        crate::events::parse(line),
        Some(
            crate::events::RuntimeEvent::ChildStarted { .. }
                | crate::events::RuntimeEvent::ChildFinished { .. }
                | crate::events::RuntimeEvent::RunletProgress { .. }
                | crate::events::RuntimeEvent::RunletTransport { .. }
        )
    ) {
        return None;
    }
    Some(format!("ACP harness {label}: {line}"))
}

async fn run(
    run_config: RunConfig,
    rx: &mut mpsc::Receiver<Request>,
    ready: oneshot::Sender<Result<Ready, String>>,
    closed: watch::Sender<bool>,
) -> Result<(), String> {
    let RunConfig {
        config,
        harness,
        persisted,
        model,
        depth,
        context,
    } = run_config;
    let permission_policy = config.harnesses.permission_policy(&harness)?;
    let mut command = config.harnesses.spawn(
        &harness,
        &config,
        persisted
            .as_ref()
            .map(|(id, resume)| (id.as_str(), *resume)),
        depth,
    )?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| context.error("spawn failure", error))?;
    let stdin = child
        .stdin
        .take()
        .ok_or("could not open ACP harness stdin")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("could not open ACP harness stdout")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("could not open ACP harness stderr")?;
    let label = harness.clone();
    let ancestor_id = config.parent_id.clone();
    let descendant_parent = ancestor_id.clone();
    tokio::spawn(async move {
        forward_stderr(
            stderr,
            &label,
            ancestor_id.as_deref(),
            |output| match output {
                ForwardedStderr::RuntimeLine(line) | ForwardedStderr::Diagnostic(line) => {
                    eprintln!("{line}");
                }
                ForwardedStderr::Cleanup(event) => crate::events::emit(&event),
            },
        )
        .await;
    });
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let routes = Arc::new(Mutex::new(
        HashMap::<SessionId, Arc<Mutex<ChildOutput>>>::new(),
    ));
    let notification_routes = Arc::clone(&routes);
    let root = config.root.clone();
    let startup_complete = Arc::new(AtomicBool::new(false));
    let ready_flag = Arc::clone(&startup_complete);
    let connected = agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                let route = notification_routes
                    .lock()
                    .ok()
                    .and_then(|routes| routes.get(&notification.session_id).cloned());
                if let Some(route) = route
                    && let Ok(mut output) = route.lock()
                {
                    output.record(notification.update);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // A headless nested client cannot ask a human. Always answer rather than
        // leaving an agent waiting forever, and choose the conservative outcome.
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                responder.respond(RequestPermissionResponse::new(permission_outcome(
                    permission_policy,
                    &request.options,
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |connection| {
            let initialized = connection.send_request(agentkit_acp::InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            let capabilities = initialized.agent_capabilities;
            let supports_close = capabilities.session_capabilities.close.is_some();
            let session = connection.send_request(agentkit_acp::NewSessionRequest::new(root.clone())).block_task().await?;
            if let Some(model) = model {
                let selectable = session.config_options.as_deref().unwrap_or_default().iter().any(|option| {
                    option.id.to_string() == "model" && matches!(option.kind, SessionConfigKind::Select(_))
                });
                if !selectable {
                    let error = format!("ACP harness {harness:?} does not advertise a selectable model session option");
                    let _ = ready.send(Err(error));
                    return std::future::pending().await;
                }
                if let Err(error) = connection.send_request(SetSessionConfigOptionRequest::new(session.session_id.clone(), "model", model.as_str())).block_task().await {
                    let error = format!("ACP harness {harness:?} rejected model selection {model:?}: {error}");
                    let _ = ready.send(Err(error));
                    return std::future::pending().await;
                }
            }
            let sessions = Arc::new(Mutex::new(vec![session.session_id.clone()]));
            let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
            let mut tasks = JoinSet::new();
            ready_flag.store(true, Ordering::Release);
            let _ = ready.send(Ok(Ready {
                session_id: session.session_id,
                capabilities,
                descendant_parent,
            }));
            let mut next_branch = 0;
            loop {
                let request = match next_actor_event(rx, &mut fatal_rx, &mut tasks, &mut next_branch).await {
                    ActorEvent::Request(Some(request)) => request,
                    ActorEvent::Request(None) => break,
                    ActorEvent::Fatal => return Err(agent_client_protocol::Error::internal_error()),
                    ActorEvent::TaskReaped => continue,
                };
                match request {
                    Request::Fork(fork) => {
                        let connection = connection.clone();
                        let sessions = Arc::clone(&sessions);
                        let root = root.clone();
                        tasks.spawn(async move {
                            let serial = fork.serial;
                            let mut request = ForkSessionRequest::new(fork.session_id, root);
                            if let Some((id, name)) = fork.parent {
                                request.meta = Some(serde_json::Map::from_iter([
                                    (FORK_PARENT_ID_META.into(), Value::String(id)),
                                    (FORK_PARENT_NAME_META.into(), Value::String(name)),
                                ]));
                            }
                            let mut request = Box::pin(connection.send_request(request).block_task());
                            // This one-shot race permits a completed fork to win
                            // simultaneous cancellation/deadline. Keep the owned
                            // request and gate for remote cleanup when it loses.
                            let result = match select(
                                &mut request,
                                select(
                                    std::pin::pin!(fork.cancellation.cancelled()),
                                    std::pin::pin!(tokio::time::sleep(HANDSHAKE)),
                                ),
                            ).await {
                                Either::Left((result, _)) => match result {
                                    Ok(response) => {
                                        let session_id = response.session_id;
                                        if let Ok(mut sessions) = sessions.lock() {
                                            sessions.push(session_id.clone());
                                        }
                                        if let Some(model) = fork.model {
                                            let selected = match tokio::time::timeout(
                                                HANDSHAKE,
                                                connection
                                                    .send_request(SetSessionConfigOptionRequest::new(
                                                        session_id.clone(),
                                                        "model",
                                                        model.as_str(),
                                                    ))
                                                    .block_task(),
                                            )
                                            .await
                                            {
                                                Ok(Ok(_)) => Ok(session_id.clone()),
                                                Ok(Err(error)) => Err(ChildError::Failed(format!(
                                                    "ACP harness rejected model selection {model:?} for forked session: {error}"
                                                ))),
                                                Err(_) => Err(ChildError::Failed(
                                                    "ACP harness did not apply the model selection to the forked session within 30 seconds".into(),
                                                )),
                                            };
                                            if selected.is_err() && supports_close {
                                                let close = connection
                                                    .send_request(CloseSessionRequest::new(session_id.clone()))
                                                    .block_task();
                                                if tokio::time::timeout(CANCEL_SETTLE, close)
                                                    .await
                                                    .is_ok_and(|result| result.is_ok())
                                                    && let Ok(mut sessions) = sessions.lock()
                                                {
                                                    sessions.retain(|id| id != &session_id);
                                                }
                                            }
                                            selected
                                        } else {
                                            Ok(session_id)
                                        }
                                    }
                                    Err(error) => Err(ChildError::Failed(error.to_string())),
                                },
                                Either::Right((Either::Left(((), _)), _)) => {
                                    let cleanup_connection = connection.clone();
                                    let cleanup_sessions = Arc::clone(&sessions);
                                    tokio::spawn(async move {
                                        // The remote fork still owns source-session
                                        // serialization until its response and cleanup.
                                        let _serial = serial;
                                        let Ok(response) = request.await else {
                                            return;
                                        };
                                        let session_id = response.session_id;
                                        let closed = supports_close
                                            && tokio::time::timeout(
                                                CANCEL_SETTLE,
                                                cleanup_connection
                                                    .send_request(CloseSessionRequest::new(session_id.clone()))
                                                    .block_task(),
                                            )
                                            .await
                                            .is_ok_and(|result| result.is_ok());
                                        if !closed
                                            && let Ok(mut sessions) = cleanup_sessions.lock()
                                        {
                                            sessions.push(session_id);
                                        }
                                    });
                                    Err(ChildError::Cancelled)
                                },
                                Either::Right((Either::Right(((), _)), _)) => {
                                    let cleanup_connection = connection.clone();
                                    let cleanup_sessions = Arc::clone(&sessions);
                                    tokio::spawn(async move {
                                        // The remote fork still owns source-session
                                        // serialization until its response and cleanup.
                                        let _serial = serial;
                                        let Ok(response) = request.await else {
                                            return;
                                        };
                                        let session_id = response.session_id;
                                        let closed = supports_close
                                            && tokio::time::timeout(
                                                CANCEL_SETTLE,
                                                cleanup_connection
                                                    .send_request(CloseSessionRequest::new(session_id.clone()))
                                                    .block_task(),
                                            )
                                            .await
                                            .is_ok_and(|result| result.is_ok());
                                        if !closed
                                            && let Ok(mut sessions) = cleanup_sessions.lock()
                                        {
                                            sessions.push(session_id);
                                        }
                                    });
                                    Err(ChildError::Failed(format!(
                                        "ACP harness did not answer session/fork within {} seconds",
                                        HANDSHAKE.as_secs()
                                    )))
                                }
                            };
                            let _ = fork.reply.send(result);
                        });
                    }
                    Request::Close(close) => {
                        let connection = connection.clone();
                        let sessions = Arc::clone(&sessions);
                        tasks.spawn(async move {
                            let request = connection
                                .send_request(CloseSessionRequest::new(close.session_id.clone()))
                                .block_task();
                            let result = tokio::time::timeout(CANCEL_SETTLE, request)
                                .await
                                .map_err(|_| {
                                    ChildError::Failed("ACP harness did not answer session/close within 5 seconds".into())
                                })
                                .and_then(|result| {
                                    result
                                        .map(|_| ())
                                        .map_err(|error| ChildError::Failed(error.to_string()))
                                });
                            if result.is_ok()
                                && let Ok(mut sessions) = sessions.lock()
                            {
                                sessions.retain(|id| id != &close.session_id);
                            }
                            let _ = close.reply.send(result);
                        });
                    }
                    Request::Prompt(prompt) => {
                        let connection = connection.clone();
                        let routes = Arc::clone(&routes);
                        let fatal = fatal_tx.clone();
                        tasks.spawn(async move {
                            let _serial = prompt.serial;
                            let session_id = prompt.session_id.clone();
                            let output = Arc::new(Mutex::new(ChildOutput::default()));
                            if let Ok(mut routes) = routes.lock() { routes.insert(session_id.clone(), Arc::clone(&output)); }
                            let request = connection.send_request(agentkit_acp::PromptRequest::new(
                                session_id.clone(), vec![ContentBlock::Text(agentkit_acp::TextContent::new(prompt.text))],
                            )).block_task();
                            tokio::pin!(request);
                            // Response-first matches the original biased race.
                            // Borrow the request so cancellation can still settle it.
                            let (response, cancelled) = match select(
                                &mut request,
                                std::pin::pin!(prompt.cancellation.cancelled()),
                            ).await {
                                Either::Left((result, _)) => (result.map_err(|error| error.to_string()), false),
                                Either::Right(((), _)) => {
                                    let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
                                    match tokio::time::timeout(CANCEL_SETTLE, &mut request).await {
                                        Ok(result) => (result.map_err(|error| error.to_string()), true),
                                        Err(_) => {
                                            let _ = prompt.reply.send(Err(ChildError::TerminalCancelled));
                                            let _ = fatal.send(());
                                            return;
                                        }
                                    }
                                }
                            };
                            if let Ok(mut routes) = routes.lock() { routes.remove(&session_id); }
                            let output = output.lock().map(|output| output.clone()).unwrap_or_default();
                            let outcome = if cancelled { Err(ChildError::Cancelled) } else {
                                response.map_err(ChildError::Failed).and_then(|response| prompt_outcome(response, output))
                            };
                            let _ = prompt.reply.send(outcome);
                        });
                    }
                }
            }
            if tokio::time::timeout(CANCEL_SETTLE, async {
                while tasks.join_next().await.is_some() {}
            }).await.is_err() {
                tasks.abort_all();
            }
            let session_ids = sessions.lock().map(|sessions| sessions.clone()).unwrap_or_default();
            if supports_close {
                for session_id in session_ids {
                    let close = connection
                        .send_request(CloseSessionRequest::new(session_id))
                        .block_task();
                    if let Ok(result) = tokio::time::timeout(CANCEL_SETTLE, close).await {
                        result?;
                    }
                }
            }
            Ok(())
        });
    tokio::pin!(connected);
    // One-shot transport/process race: EOF may already win before reaping.
    // Keep the post-race exit-status check and await transport after process exit.
    let connected = match select(&mut connected, std::pin::pin!(child.wait())).await {
        Either::Left((result, _)) => result,
        Either::Right((status, _)) => {
            let status = status.map_err(|error| context.error("process status failure", error))?;
            let _ = closed.send(true);
            if !startup_complete.load(Ordering::Acquire) && !status.success() {
                return Err(pre_handshake_exit(&context, status));
            }
            connected.await
        }
    };
    if !startup_complete.load(Ordering::Acquire) {
        // Transport EOF can win the race with process reaping. Briefly wait for a
        // failing status so launch failures keep their actionable exit details.
        match tokio::time::timeout(PRE_HANDSHAKE_EXIT_SETTLE, child.wait()).await {
            Ok(Ok(status)) if !status.success() => {
                return Err(pre_handshake_exit(&context, status));
            }
            Ok(Err(error)) => return Err(context.error("process status failure", error)),
            Ok(Ok(_)) | Err(_) => {}
        }
    }
    let _ = child.kill().await;
    connected.map_err(|error| {
        if startup_complete.load(Ordering::Acquire) {
            error.to_string()
        } else {
            // ACP error messages and data are child-controlled and may contain
            // secrets. Keep launch diagnostics to a fixed local reason.
            context.error(
                "protocol handshake failure",
                "the child did not complete the ACP handshake",
            )
        }
    })
}

#[derive(Debug, PartialEq)]
enum ForwardedStderr {
    RuntimeLine(String),
    Diagnostic(String),
    Cleanup(crate::events::RuntimeEvent),
}

async fn forward_stderr(
    stderr: impl AsyncRead + Unpin,
    label: &str,
    ancestor_id: Option<&str>,
    mut output: impl FnMut(ForwardedStderr),
) {
    let mut ancestors = ancestor_id
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(event) = crate::events::parse(&line)
            && event.forward_from_child()
        {
            if let crate::events::RuntimeEvent::SubagentStateChanged {
                parent_id: Some(parent_id),
                ..
            } = event
            {
                ancestors.insert(parent_id);
            }
            // Preserve recursively forwarded private runtime events byte-for-byte.
            output(ForwardedStderr::RuntimeLine(line));
        } else if let Some(line) = harness_diagnostic(label, &line) {
            output(ForwardedStderr::Diagnostic(line));
        }
    }
    for ancestor_id in ancestors {
        output(ForwardedStderr::Cleanup(
            crate::events::RuntimeEvent::SubagentDescendantsRemoved { ancestor_id },
        ));
    }
}

fn pre_handshake_exit(context: &LaunchContext, status: std::process::ExitStatus) -> String {
    context.error(
        "pre-handshake exit",
        format!("child exited with {status} before completing the ACP handshake"),
    )
}

fn permission_outcome(
    policy: AcpPermissionPolicy,
    options: &[PermissionOption],
) -> RequestPermissionOutcome {
    if policy == AcpPermissionPolicy::Deny
        && let Some(option) = options
            .iter()
            .find(|option| option.kind == PermissionOptionKind::RejectAlways)
            .or_else(|| {
                options
                    .iter()
                    .find(|option| option.kind == PermissionOptionKind::RejectOnce)
            })
    {
        return RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        ));
    }
    RequestPermissionOutcome::Cancelled
}

fn prompt_outcome(
    response: PromptResponse,
    output: ChildOutput,
) -> Result<ChildOutput, ChildError> {
    match response.stop_reason {
        StopReason::EndTurn | StopReason::MaxTokens => Ok(output),
        StopReason::Cancelled => Err(ChildError::Cancelled),
        StopReason::Refusal => Err(ChildError::Failed("nested agent refused the prompt".into())),
        StopReason::MaxTurnRequests => Err(ChildError::Failed(
            "nested agent reached its turn-request limit".into(),
        )),
        _ => Err(ChildError::Failed(
            "nested agent returned an unknown stop reason".into(),
        )),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod test_support {
    use super::*;

    impl ChildSession {
        pub(crate) fn closure_probe_for_test() -> (Self, oneshot::Receiver<()>) {
            let (tx, mut rx) = mpsc::channel(1);
            let (closed_tx, closed_rx) = oneshot::channel();
            tokio::spawn(async move {
                while rx.recv().await.is_some() {}
                let _ = closed_tx.send(());
            });
            (
                Self {
                    tx,
                    session_id: "test".into(),
                    capabilities: agentkit_acp::AgentCapabilities::default(),
                    serial: Arc::new(tokio::sync::Mutex::new(())),
                    closed: watch::channel(false).1,
                    descendant_parent: None,
                },
                closed_rx,
            )
        }

        pub(crate) fn disconnected_for_test() -> Self {
            let (tx, rx) = mpsc::channel(1);
            drop(rx);
            Self {
                tx,
                session_id: "test".into(),
                capabilities: agentkit_acp::AgentCapabilities::default(),
                serial: Arc::new(tokio::sync::Mutex::new(())),
                closed: watch::channel(false).1,
                descendant_parent: None,
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use serde_json::json;

    use super::*;

    fn update(value: Value) -> SessionUpdate {
        serde_json::from_value(value).unwrap()
    }

    fn admission_test_session() -> (ChildSession, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::channel(1);
        let mut capabilities = agentkit_acp::AgentCapabilities::default();
        capabilities.session_capabilities.fork = Some(Default::default());
        (
            ChildSession {
                tx,
                session_id: "test".into(),
                capabilities,
                serial: Arc::new(tokio::sync::Mutex::new(())),
                closed: watch::channel(false).1,
                descendant_parent: None,
            },
            rx,
        )
    }

    #[tokio::test]
    async fn actor_events_share_ready_request_fatal_and_completion_backlogs() {
        for enabled in [0b011u8, 0b101, 0b110, 0b111] {
            for first_branch in 0..3 {
                let (tx, mut rx) = mpsc::channel(8);
                let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
                let mut tasks = JoinSet::new();
                let mut replies = Vec::new();
                let mut completed = Vec::new();
                for id in 0..8 {
                    if enabled & 1 != 0 {
                        let (reply, response) = oneshot::channel();
                        tx.send(Request::Close(Close {
                            session_id: id.to_string().into(),
                            reply,
                        }))
                        .await
                        .unwrap();
                        replies.push(response);
                    }
                    if enabled & 2 != 0 {
                        fatal_tx.send(()).unwrap();
                    }
                    if enabled & 4 != 0 {
                        completed.push(tasks.spawn(async {}));
                    }
                }
                // Establish simultaneous readiness through real task handles, not
                // timing assumptions or a replacement scheduler.
                for task in completed {
                    while !task.is_finished() {
                        tokio::task::yield_now().await;
                    }
                }
                let mut next_branch = first_branch;
                for id in 0..8 {
                    let mut seen = 0;
                    // Each continuously ready source gets a turn, including when
                    // the cursor starts at a pending source. This asserts the
                    // arbitration contract, not internal poll counts.
                    for _ in 0..enabled.count_ones() {
                        match next_actor_event(&mut rx, &mut fatal_rx, &mut tasks, &mut next_branch)
                            .await
                        {
                            ActorEvent::Request(Some(Request::Close(close))) => {
                                assert_eq!(seen & 1, 0);
                                seen |= 1;
                                assert_eq!(close.session_id, SessionId::from(id.to_string()));
                                close.reply.send(Ok(())).unwrap();
                            }
                            ActorEvent::Fatal => {
                                assert_eq!(seen & 2, 0);
                                seen |= 2;
                            }
                            ActorEvent::TaskReaped => {
                                assert_eq!(seen & 4, 0);
                                seen |= 4;
                            }
                            _ => panic!("unexpected actor event"),
                        }
                    }
                    assert_eq!(seen, enabled);
                }
                for reply in replies {
                    reply.await.unwrap().unwrap();
                }
                assert!(tasks.is_empty());
                drop(tx);
                assert!(matches!(
                    next_actor_event(&mut rx, &mut fatal_rx, &mut tasks, &mut next_branch).await,
                    ActorEvent::Request(None)
                ));
            }
        }
    }

    #[tokio::test]
    async fn actor_events_wake_for_each_live_source_and_ignore_closed_sources() {
        for source in 0..3 {
            let (tx, mut rx) = mpsc::channel(1);
            let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
            let mut tasks = JoinSet::new();
            let (finish, finished) = oneshot::channel();
            if source == 2 {
                tasks.spawn(async move {
                    finished.await.unwrap();
                });
            }
            // A closed fatal channel and an empty task set must not spin or
            // prevent the request channel from registering a wakeup.
            let fatal_tx = (source == 1).then_some(fatal_tx);
            let (waiting, wait) = oneshot::channel();
            let event = tokio::spawn(async move {
                let mut next_branch = 1;
                let mut event = Box::pin(next_actor_event(
                    &mut rx,
                    &mut fatal_rx,
                    &mut tasks,
                    &mut next_branch,
                ));
                assert!(futures_util::poll!(&mut event).is_pending());
                waiting.send(()).unwrap();
                event.await
            });
            // The arbiter is suspended before making a source ready. Its own
            // registered waker, not a manual re-poll, must resume the task.
            wait.await.unwrap();
            match source {
                0 => {
                    let (reply, _response) = oneshot::channel();
                    tx.send(Request::Close(Close {
                        session_id: "wake".into(),
                        reply,
                    }))
                    .await
                    .unwrap();
                }
                1 => fatal_tx.unwrap().send(()).unwrap(),
                _ => finish.send(()).unwrap(),
            }
            let event = tokio::time::timeout(Duration::from_secs(5), event)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                (source, event),
                (0, ActorEvent::Request(Some(_)))
                    | (1, ActorEvent::Fatal)
                    | (2, ActorEvent::TaskReaped)
            ));
        }
    }

    #[tokio::test]
    async fn actor_arbitration_leaves_losing_request_and_serialization_owned() {
        let (child, mut rx) = admission_test_session();
        let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
        let mut tasks = JoinSet::new();
        let serial = child.serial.clone().lock_owned().await;
        let (reply, mut response) = oneshot::channel();
        let controller = agentkit_core::CancellationController::new();
        child
            .tx
            .send(Request::Prompt(Prompt {
                serial,
                session_id: child.session_id.clone(),
                text: "queued".into(),
                cancellation: controller.handle().checkpoint(),
                reply,
            }))
            .await
            .unwrap();
        controller.interrupt();
        fatal_tx.send(()).unwrap();
        let mut next_branch = 1;
        assert!(matches!(
            next_actor_event(&mut rx, &mut fatal_rx, &mut tasks, &mut next_branch).await,
            ActorEvent::Fatal
        ));
        assert!(child.serial.try_lock().is_err());
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        // Fatal shutdown drops the queued request, releasing both the reply and
        // its serialization claim exactly as dropping the actor does.
        drop(rx);
        assert!(response.await.is_err());
        assert!(child.serial.try_lock().is_ok());
    }

    #[tokio::test]
    async fn cancelled_admission_releases_unsent_serialization() {
        for fork in [false, true] {
            for waiting_for_gate in [false, true] {
                let (child, mut rx) = admission_test_session();
                let guard = if waiting_for_gate {
                    Some(child.serial.clone().lock_owned().await)
                } else {
                    None
                };
                // Reserve all channel capacity without giving the actor a request.
                let capacity = child.tx.reserve().await.unwrap();
                let controller = agentkit_core::CancellationController::new();
                let cancellation = controller.handle().checkpoint();
                let mut operation = Box::pin(async {
                    if fork {
                        child.fork(None, None, &cancellation).await.map(|_| ())
                    } else {
                        child
                            .prompt("blocked".into(), cancellation.clone())
                            .await
                            .map(|_| ())
                    }
                });
                assert!(futures_util::poll!(&mut operation).is_pending());
                assert!(child.serial.try_lock().is_err());
                controller.interrupt();
                assert!(matches!(operation.await, Err(ChildError::Cancelled)));
                assert!(matches!(
                    rx.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                drop(guard);
                assert!(child.serial.try_lock().is_ok());
                drop(capacity);
                assert!(child.tx.try_reserve().is_ok());
            }
        }
    }

    #[tokio::test]
    async fn ready_admission_keeps_cancellation_with_actor_owned_request() {
        for fork in [false, true] {
            let (child, mut rx) = admission_test_session();
            let controller = agentkit_core::CancellationController::new();
            let cancellation = controller.handle().checkpoint();
            controller.interrupt();
            let operation = async {
                if fork {
                    child.fork(None, None, &cancellation).await.map(|_| ())
                } else {
                    child
                        .prompt("ready".into(), cancellation.clone())
                        .await
                        .map(|_| ())
                }
            };
            let answer = async {
                let request = rx.recv().await.unwrap();
                assert!(child.serial.try_lock().is_err());
                match request {
                    Request::Fork(fork) => {
                        assert!(
                            futures_util::poll!(std::pin::pin!(fork.cancellation.cancelled()))
                                .is_ready()
                        );
                        fork.reply.send(Err(ChildError::Cancelled)).unwrap();
                    }
                    Request::Prompt(prompt) => {
                        assert!(
                            futures_util::poll!(std::pin::pin!(prompt.cancellation.cancelled()))
                                .is_ready()
                        );
                        prompt.reply.send(Err(ChildError::Cancelled)).unwrap();
                    }
                    Request::Close(_) => panic!("expected prompt or fork"),
                }
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(operation, answer)
            })
            .await
            .unwrap();
            assert!(matches!(result, Err(ChildError::Cancelled)));
            assert!(child.serial.try_lock().is_ok());
        }
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_release_child_request_serialization() {
        for fork in [false, true] {
            let (child, mut rx) = admission_test_session();
            let caller = child.clone();
            let task = tokio::spawn(async move {
                if fork {
                    caller
                        .fork(None, None, &TurnCancellation::default())
                        .await
                        .map(|_| ())
                } else {
                    caller
                        .prompt("first".into(), TurnCancellation::default())
                        .await
                        .map(|_| ())
                }
            });
            // The actor has accepted the request but has not settled it.
            let request = rx.recv().await.unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(child.serial.try_lock().is_err());
            drop(request);
            assert!(child.serial.try_lock().is_ok());

            // Settlement releases the same gate for a subsequent prompt.
            let answer = async {
                let Some(Request::Prompt(prompt)) = rx.recv().await else {
                    panic!("expected a prompt");
                };
                prompt.reply.send(Ok(ChildOutput::default())).unwrap();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(
                    child.prompt("next".into(), TurnCancellation::default()),
                    answer
                )
            })
            .await
            .unwrap();
            assert!(result.is_ok());
        }
    }

    #[test]
    fn nested_runtime_events_are_not_forwarded_as_parent_events() {
        let event = crate::events::RuntimeEvent::ChildStarted {
            call: "subagent-call:compose:shell".into(),
            tool: "shell".into(),
            summary: "inspect".into(),
            at: 0,
        };
        let line = format!(
            "{}{}",
            crate::events::EVENT_MARKER,
            serde_json::to_string(&event).unwrap()
        );

        assert_eq!(harness_diagnostic("kit", &line), None);
        assert_eq!(
            harness_diagnostic("kit", "ordinary diagnostic").as_deref(),
            Some("ACP harness kit: ordinary diagnostic")
        );
    }

    #[test]
    fn tui_serve_command_forwards_resolved_reasoning_effort() {
        let root = tempfile::tempdir().unwrap();
        let command = serve_command(
            root.path(),
            "test-model",
            crate::ProviderKind::OpenRouter,
            Some(crate::ReasoningEffort::Medium),
            Some(&crate::provider::OpenRouterApiKey::new("tui-secret")),
            "session",
            true,
        )
        .unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--reasoning-effort", "medium"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--stdio-protocol-version", "2"])
        );
        assert!(args.iter().all(|arg| arg != "tui-secret"));
        assert!(command.as_std().get_envs().any(|(name, value)| {
            name == "OPENROUTER_API_KEY" && value == Some(std::ffi::OsStr::new("tui-secret"))
        }));
    }

    #[test]
    fn openrouter_key_is_removed_from_external_acp_profiles() {
        let root = tempfile::tempdir().unwrap();
        let harnesses = AcpHarnesses::new(BTreeMap::from([(
            "external".into(),
            AcpHarnessProfile {
                command: "external-agent".into(),
                args: Vec::new(),
                permissions: AcpPermissionPolicy::Deny,
            },
        )]))
        .unwrap();
        for openrouter_api_key in [
            Some(crate::provider::OpenRouterApiKey::new("external-secret")),
            None,
        ] {
            let config = ChildConfig {
                root: root.path().into(),
                model: "model".into(),
                provider: crate::ProviderKind::OpenRouter,
                reasoning_effort: None,
                openrouter_api_key,
                configured_mcp_config: None,
                configured_mcp_config_inherited: false,
                legacy_mcp_config: false,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses: harnesses.clone(),
                default_harness: "acp.external".into(),
                parent_id: None,
                parent_name: None,
            };
            let command = harnesses.spawn("acp.external", &config, None, 1).unwrap();
            assert!(
                command
                    .as_std()
                    .get_envs()
                    .any(|(name, value)| { name == "OPENROUTER_API_KEY" && value.is_none() })
            );
        }
    }

    #[test]
    fn captures_text_separately_from_safe_rich_updates() {
        let mut output = ChildOutput::default();
        output.record(update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"}
        })));
        output.record(update(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "private reasoning"}
        })));
        output.record(update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}
        })));
        output.record(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-1",
            "title": "Inspect files"
        })));
        output.record(update(json!({
            "sessionUpdate": "plan",
            "entries": [{"content": "Inspect", "priority": "high", "status": "pending"}]
        })));

        assert_eq!(output.text, "hello");
        assert_eq!(output.updates.len(), 3);
        assert_eq!(output.updates[0]["content"]["type"], "image");
        assert_eq!(output.updates[1]["sessionUpdate"], "tool_call");
        assert_eq!(output.updates[2]["sessionUpdate"], "plan");
        assert!(!output.updates_truncated);
    }

    #[test]
    fn captured_tool_updates_drop_content_that_duplicates_raw_output() {
        let raw = json!({"exit_code": 0, "stdout": "done", "stderr": "", "success": true});
        let mut output = ChildOutput::default();
        output.record(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "status": "completed",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": serde_json::to_string(&raw).unwrap()}
            }],
            "rawOutput": raw
        })));

        assert_eq!(output.updates.len(), 1);
        assert!(output.updates[0].get("content").is_none());
        assert_eq!(output.updates[0]["rawOutput"]["stdout"], "done");
    }

    #[test]
    fn captured_tool_updates_keep_distinct_content_and_raw_output() {
        let mut output = ChildOutput::default();
        output.record(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": "short summary"}
            }],
            "rawOutput": {"stdout": "full output"}
        })));

        assert!(output.updates[0].get("content").is_some());
        assert!(output.updates[0].get("rawOutput").is_some());
    }

    #[test]
    fn captured_tool_updates_keep_other_content_beside_a_duplicate_text_block() {
        let raw = json!({"stdout": "full output"});
        let mut output = ChildOutput::default();
        output.record(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "content": [
                {
                    "type": "content",
                    "content": {"type": "text", "text": serde_json::to_string(&raw).unwrap()}
                },
                {"type": "diff", "path": "src/lib.rs", "newText": "changed"}
            ],
            "rawOutput": raw
        })));

        assert!(output.updates[0].get("content").is_some());
    }

    #[test]
    fn rich_updates_are_bounded_by_count_and_bytes() {
        let image = || {
            update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "image", "data": "eA==", "mimeType": "image/png"}
            }))
        };
        let mut counted = ChildOutput::default();
        for _ in 0..=MAX_CAPTURED_UPDATES {
            counted.record(image());
        }
        assert_eq!(counted.updates.len(), MAX_CAPTURED_UPDATES);
        assert!(counted.updates_truncated);

        let mut oversized = ChildOutput::default();
        oversized.record(update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {
                "type": "image",
                "data": "x".repeat(MAX_CAPTURED_UPDATE_BYTES),
                "mimeType": "image/png"
            }
        })));
        assert!(oversized.updates.is_empty());
        assert!(oversized.updates_truncated);
    }

    #[test]
    fn launch_context_omits_command_arguments_and_root_path() {
        let profiles = BTreeMap::from([(
            "safe-name".into(),
            AcpHarnessProfile {
                command: "secret-command-name".into(),
                args: vec!["secret-argument".into()],
                permissions: AcpPermissionPolicy::Deny,
            },
        )]);
        let harnesses = AcpHarnesses::new(profiles).unwrap();
        let context = harnesses.launch_context("acp.safe-name");
        let error = context.error("handshake timeout", "no response within 30 seconds");
        assert!(error.contains("harness=\"acp.safe-name\""));
        assert!(error.contains("source=configured ACP profile"));
        assert!(error.contains("cwd=configured working directory"));
        assert!(!error.contains("secret-command-name"));
        assert!(!error.contains("secret-argument"));
        assert!(!error.contains("/private/runtime/root"));

        assert_eq!(
            AcpHarnesses::default()
                .launch_context(BUILTIN_HARNESS)
                .source,
            "built-in current executable"
        );
    }

    #[tokio::test]
    async fn protocol_handshake_failure_includes_only_safe_launch_context() {
        let root = tempfile::tempdir().unwrap();
        let profiles = BTreeMap::from([(
            "broken".into(),
            AcpHarnessProfile {
                command: "python3".into(),
                args: vec![
                    "-c".into(),
                    concat!(
                        "import json,sys; ",
                        "request=json.loads(sys.stdin.readline()); ",
                        "response={'jsonrpc':'2.0','id':request['id'],'error':",
                        "{'code':-32000,'message':'remote-secret-message',",
                        "'data':{'token':'remote-secret-data'}}}; ",
                        "print(json.dumps(response), flush=True)"
                    )
                    .into(),
                ],
                permissions: AcpPermissionPolicy::Deny,
            },
        )]);
        let config = ChildConfig {
            root: root.path().into(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            configured_mcp_config: None,
            configured_mcp_config_inherited: false,
            legacy_mcp_config: false,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses: AcpHarnesses::new(profiles).unwrap(),
            default_harness: "acp.broken".into(),
            parent_id: None,
            parent_name: None,
        };
        let result = ChildSession::start(
            config,
            "acp.broken".into(),
            None,
            None,
            1,
            TurnCancellation::default(),
        )
        .await;
        let error = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("harness unexpectedly completed its handshake"),
        };
        assert!(error.contains("protocol handshake failure"), "{error}");
        assert!(error.contains("harness=\"acp.broken\""), "{error}");
        assert!(error.contains("source=configured ACP profile"), "{error}");
        assert!(
            error.contains("cwd=configured working directory"),
            "{error}"
        );
        assert!(
            error.contains("the child did not complete the ACP handshake"),
            "{error}"
        );
        assert!(!error.contains("python3"), "{error}");
        assert!(!error.contains("remote-secret-message"), "{error}");
        assert!(!error.contains("remote-secret-data"), "{error}");
        assert!(!error.contains("token"), "{error}");
        assert!(!error.contains(root.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn pre_handshake_exit_includes_status_and_safe_launch_context() {
        let root = tempfile::tempdir().unwrap();
        let profiles = BTreeMap::from([(
            "exits".into(),
            AcpHarnessProfile {
                command: "python3".into(),
                // Make transport EOF precede process exit to exercise the reaping race.
                args: vec![
                    "-c".into(),
                    "import os, time; os.close(1); time.sleep(0.05); raise SystemExit(17)".into(),
                ],
                permissions: AcpPermissionPolicy::Deny,
            },
        )]);
        let config = ChildConfig {
            root: root.path().into(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            configured_mcp_config: None,
            configured_mcp_config_inherited: false,
            legacy_mcp_config: false,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses: AcpHarnesses::new(profiles).unwrap(),
            default_harness: "acp.exits".into(),
            parent_id: None,
            parent_name: None,
        };

        let error = match ChildSession::start(
            config,
            "acp.exits".into(),
            None,
            None,
            1,
            TurnCancellation::default(),
        )
        .await
        {
            Err(error) => error.to_string(),
            Ok(_) => panic!("harness unexpectedly completed its handshake"),
        };

        assert!(error.contains("pre-handshake exit"), "{error}");
        assert!(error.contains("17"), "{error}");
        assert!(error.contains("harness=\"acp.exits\""), "{error}");
        assert!(error.contains("source=configured ACP profile"), "{error}");
        assert!(
            error.contains("cwd=configured working directory"),
            "{error}"
        );
        assert!(!error.contains("python3"), "{error}");
        assert!(!error.contains("raise SystemExit"), "{error}");
        assert!(!error.contains(root.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn configured_profile_is_spawned_as_literal_argv_at_the_root() {
        let root = tempfile::tempdir().unwrap();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "other".into(),
            AcpHarnessProfile {
                command: "agent binary".into(),
                args: vec!["two words".into(), "; not a shell".into()],
                permissions: AcpPermissionPolicy::Deny,
            },
        );
        let harnesses = AcpHarnesses::new(profiles).unwrap();
        let config = ChildConfig {
            root: root.path().to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            configured_mcp_config: None,
            configured_mcp_config_inherited: false,
            legacy_mcp_config: false,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses: harnesses.clone(),
            default_harness: "acp.other".into(),
            parent_id: None,
            parent_name: None,
        };
        let command = harnesses.spawn("acp.other", &config, None, 0).unwrap();
        assert_eq!(command.as_std().get_program(), "agent binary");
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["two words", "; not a shell"]
        );
        assert_eq!(command.as_std().get_current_dir(), Some(root.path()));
        assert!(!harnesses.contains("other"));
    }

    #[test]
    fn configured_acp_kit_keeps_kit_invariants_and_fallback_identity() {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "kit".into(),
            AcpHarnessProfile {
                command: "kit".into(),
                args: vec!["acp".into()],
                permissions: AcpPermissionPolicy::Deny,
            },
        );
        let harnesses = AcpHarnesses::new(profiles).unwrap();
        assert!(harnesses.contains("acp.kit"));
        assert!(harnesses.is_kit("acp.kit"));
        let root = tempfile::tempdir().unwrap();
        let configured_directory = tempfile::tempdir().unwrap();
        let configured_mcp = configured_directory.path().join("configured.json");
        assert!(configured_mcp.is_absolute());
        assert!(!configured_mcp.starts_with(root.path()));
        let config = ChildConfig {
            root: root.path().to_path_buf(),
            model: "test-model".into(),
            provider: crate::ProviderKind::OpenRouter,
            reasoning_effort: Some(crate::ReasoningEffort::High),
            openrouter_api_key: Some(crate::provider::OpenRouterApiKey::new("child-secret")),
            configured_mcp_config: Some(configured_mcp.clone()),
            configured_mcp_config_inherited: true,
            legacy_mcp_config: false,
            mcp_config: None,
            credential_storage: CredentialStorage::Filesystem(root.path().join("credentials")),
            telemetry: crate::telemetry::Settings::try_new_with_protocol(
                Some("http://collector:4318".into()),
                crate::telemetry::Protocol::HttpJson,
                false,
                12,
                4096,
            )
            .unwrap(),
            harnesses: harnesses.clone(),
            default_harness: BUILTIN_HARNESS.into(),
            parent_id: None,
            parent_name: None,
        };
        let command = harnesses
            .spawn("acp.kit", &config, Some(("session", true)), 2)
            .unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(command.as_std().get_program(), "kit");
        assert_eq!(args[0], "acp");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "test-model"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--provider", "openrouter"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--reasoning-effort", "high"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--session-id", "session"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--subagent-depth", "2"])
        );
        assert!(args.iter().any(|arg| arg == "--root"));
        assert!(args.iter().any(|arg| arg == "--resume"));
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--internal-mcp-config" && pair[1] == configured_mcp.to_string_lossy()
        }));
        assert!(args.iter().all(|arg| arg != "--mcp-config"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--credential-store", "file"])
        );
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--credential-dir"
                && pair[1] == root.path().join("credentials").to_string_lossy()
        }));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--otel-endpoint", "http://collector:4318/v1/traces"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--otel-protocol", "http/json"])
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--otel-capture-message-content", "false"] })
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--otel-message-content-max-messages", "12"] })
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--otel-message-content-max-bytes", "4096"] })
        );
        assert_eq!(command.as_std().get_current_dir(), Some(root.path()));
        assert!(args.iter().all(|arg| arg != "child-secret"));
        assert!(command.as_std().get_envs().any(|(name, value)| {
            name == "OPENROUTER_API_KEY" && value == Some(std::ffi::OsStr::new("child-secret"))
        }));

        let mut no_configured = config.clone();
        no_configured.configured_mcp_config = None;
        let command = harnesses
            .spawn("acp.kit", &no_configured, Some(("session", false)), 2)
            .unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "--internal-no-mcp-config"));
        assert!(args.iter().all(|arg| arg != "--internal-mcp-config"));

        let mut legacy = config;
        legacy.configured_mcp_config = None;
        legacy.configured_mcp_config_inherited = false;
        legacy.legacy_mcp_config = true;
        legacy.mcp_config = Some(PathBuf::from("/legacy/explicit.json"));
        let command = harnesses
            .spawn("acp.kit", &legacy, Some(("session", false)), 2)
            .unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "--internal-mcp-legacy"));
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--mcp-config", "/legacy/explicit.json"] })
        );
        assert!(args.iter().all(|arg| arg != "--internal-no-mcp-config"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_startup_failures_do_not_poll_join_handles_twice() {
        let root = tempfile::tempdir().unwrap();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "broken".into(),
            AcpHarnessProfile {
                command: "kit-test-acp-executable-that-does-not-exist".into(),
                args: Vec::new(),
                permissions: AcpPermissionPolicy::Deny,
            },
        );
        let config = ChildConfig {
            root: root.path().to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            configured_mcp_config: None,
            configured_mcp_config_inherited: false,
            legacy_mcp_config: false,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses: AcpHarnesses::new(profiles).unwrap(),
            default_harness: "acp.broken".into(),
            parent_id: None,
            parent_name: None,
        };
        let starts = (0..64)
            .map(|_| {
                let config = config.clone();
                tokio::spawn(async move {
                    ChildSession::start(
                        config,
                        "acp.broken".into(),
                        None,
                        None,
                        1,
                        TurnCancellation::default(),
                    )
                    .await
                })
            })
            .collect::<Vec<_>>();

        for start in starts {
            let result = start.await.expect("child startup must not panic");
            let Err(ChildError::Failed(error)) = result else {
                panic!("expected a spawn failure");
            };
            assert!(error.contains("spawn failure"), "{error}");
            assert!(error.contains("harness=\"acp.broken\""), "{error}");
            assert!(error.contains("source=configured ACP profile"), "{error}");
            assert!(
                error.contains("cwd=configured working directory"),
                "{error}"
            );
            assert!(!error.contains("kit-test-acp-executable-that-does-not-exist"));
            assert!(!error.contains(root.path().to_string_lossy().as_ref()));
        }
    }

    #[tokio::test]
    async fn mock_stdio_agent_prompts_and_native_forks_concurrently() {
        let root = tempfile::tempdir().unwrap();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "mock".into(),
            AcpHarnessProfile {
                command: "python3".into(),
                args: vec![format!(
                    "{}/fixtures/mock-acp.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                permissions: AcpPermissionPolicy::Deny,
            },
        );
        let harnesses = AcpHarnesses::new(profiles).unwrap();
        let config = ChildConfig {
            root: root.path().to_path_buf(),
            model: "unused".into(),
            provider: Default::default(),
            reasoning_effort: None,
            openrouter_api_key: None,
            configured_mcp_config: None,
            configured_mcp_config_inherited: false,
            legacy_mcp_config: false,
            mcp_config: None,
            credential_storage: Default::default(),
            telemetry: Default::default(),
            harnesses,
            default_harness: "acp.mock".into(),
            parent_id: None,
            parent_name: None,
        };
        let base = ChildSession::start(
            config,
            "acp.mock".into(),
            None,
            None,
            1,
            TurnCancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            base.prompt("standard".into(), TurnCancellation::default())
                .await
                .unwrap()
                .text,
            "standard"
        );
        let closed = base
            .fork(None, None, &TurnCancellation::default())
            .await
            .unwrap();
        closed.close().await.unwrap();
        assert_eq!(
            base.prompt("after sibling close".into(), TurnCancellation::default())
                .await
                .unwrap()
                .text,
            "after sibling close"
        );

        let first = base
            .fork(None, None, &TurnCancellation::default())
            .await
            .unwrap();
        let second = base
            .fork(None, None, &TurnCancellation::default())
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let (first, second) = tokio::join!(
            first.prompt("first".into(), TurnCancellation::default()),
            second.prompt("second".into(), TurnCancellation::default()),
        );
        assert_eq!(first.unwrap().text, "first");
        assert_eq!(second.unwrap().text, "second");
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "native fork sibling prompts were serialized: {:?}",
            started.elapsed()
        );

        let same_session = base
            .fork(None, None, &TurnCancellation::default())
            .await
            .unwrap();
        let same_session_clone = same_session.clone();
        let started = std::time::Instant::now();
        let (first, second) = tokio::join!(
            same_session.prompt("same-first".into(), TurnCancellation::default()),
            same_session_clone.prompt("same-second".into(), TurnCancellation::default()),
        );
        assert_eq!(first.unwrap().text, "same-first");
        assert_eq!(second.unwrap().text, "same-second");
        assert!(
            started.elapsed() >= Duration::from_millis(750),
            "one logical session was not serialized: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn actor_fatal_shutdown_progresses_during_sibling_request_pressure() {
        // This is a deadlock watchdog, not a performance assertion. The peer
        // never settles one cancelled prompt, while serving sibling requests.
        tokio::time::timeout(Duration::from_secs(20), async {
            let root = tempfile::tempdir().unwrap();
            let log = root.path().join("requests.jsonl");
            let harnesses = AcpHarnesses::new(BTreeMap::from([(
                "mock".into(),
                AcpHarnessProfile {
                    command: "python3".into(),
                    args: vec![
                        format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR")),
                        format!("--request-log={}", log.display()),
                        format!("--prompt-release={}", root.path().join("never").display()),
                        "--prompt-release-text=held".into(),
                    ],
                    permissions: AcpPermissionPolicy::Deny,
                },
            )]))
            .unwrap();
            let base = ChildSession::start(
                ChildConfig {
                    root: root.path().to_path_buf(),
                    model: "unused".into(),
                    provider: Default::default(),
                    reasoning_effort: None,
                    openrouter_api_key: None,
                    configured_mcp_config: None,
                    configured_mcp_config_inherited: false,
                    legacy_mcp_config: false,
                    mcp_config: None,
                    credential_storage: Default::default(),
                    telemetry: Default::default(),
                    harnesses,
                    default_harness: "acp.mock".into(),
                    parent_id: None,
                    parent_name: None,
                },
                "acp.mock".into(),
                None,
                None,
                1,
                TurnCancellation::default(),
            )
            .await
            .unwrap();
            let mut producers = JoinSet::new();
            let mut started = Vec::new();
            for _ in 0..4 {
                let sibling = base
                    .fork(None, None, &TurnCancellation::default())
                    .await
                    .unwrap();
                let (ready, wait) = oneshot::channel();
                started.push(wait);
                producers.spawn(async move {
                    let mut ready = Some(ready);
                    loop {
                        match sibling
                            .prompt("flowing".into(), TurnCancellation::default())
                            .await
                        {
                            Ok(output) => {
                                assert_eq!(output.text, "flowing");
                                if let Some(ready) = ready.take() {
                                    ready.send(()).unwrap();
                                }
                            }
                            Err(error) => return error,
                        }
                    }
                });
            }
            for ready in started {
                ready.await.unwrap();
            }
            let controller = agentkit_core::CancellationController::new();
            let cancellation = controller.handle().checkpoint();
            let child = base.clone();
            let held = tokio::spawn(async move { child.prompt("held".into(), cancellation).await });
            // Observe acceptance at the real protocol boundary before cancelling.
            while !std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("\"text\":\"held\"")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            controller.interrupt();
            assert!(matches!(
                held.await.unwrap(),
                Err(ChildError::TerminalCancelled)
            ));
            let mut closed = base.closed.clone();
            closed.wait_for(|closed| *closed).await.unwrap();
            // Producers stop only because the actor shuts down, not because the
            // test withdraws pressure. Outstanding callers must also settle.
            while let Some(result) = producers.join_next().await {
                assert!(matches!(result.unwrap(), ChildError::TerminalFailed(_)));
            }
            assert!(base.serial.try_lock().is_ok());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_or_timed_out_fork_retains_serialization_until_remote_settlement() {
        for cancel in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let release = root.path().join("release-fork");
            let mut profiles = BTreeMap::new();
            profiles.insert(
                "mock".into(),
                AcpHarnessProfile {
                    command: "python3".into(),
                    args: vec![
                        format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR")),
                        format!("--fork-release={}", release.display()),
                    ],
                    permissions: AcpPermissionPolicy::Deny,
                },
            );
            let config = ChildConfig {
                root: root.path().to_path_buf(),
                model: "unused".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                configured_mcp_config: None,
                configured_mcp_config_inherited: false,
                legacy_mcp_config: false,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses: AcpHarnesses::new(profiles).unwrap(),
                default_harness: "acp.mock".into(),
                parent_id: None,
                parent_name: None,
            };
            let base = ChildSession::start(
                config,
                "acp.mock".into(),
                None,
                None,
                1,
                TurnCancellation::default(),
            )
            .await
            .unwrap();
            let controller = agentkit_core::CancellationController::new();
            let cancellation = controller.handle().checkpoint();
            let child = base.clone();
            let fork = tokio::spawn(async move { child.fork(None, None, &cancellation).await });
            tokio::time::sleep(Duration::from_millis(100)).await;
            if cancel {
                controller.interrupt();
            }
            let outcome = tokio::time::timeout(HANDSHAKE + Duration::from_secs(5), fork)
                .await
                .unwrap()
                .unwrap();
            if cancel {
                assert!(matches!(outcome, Err(ChildError::Cancelled)));
            } else {
                assert!(matches!(outcome, Err(ChildError::Failed(_))));
            }
            assert!(base.serial.try_lock().is_err());
            let mut next = Box::pin(base.prompt(
                "source survives cancellation".into(),
                TurnCancellation::default(),
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut next)
                    .await
                    .is_err()
            );
            std::fs::write(release, b"ready").unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), next)
                    .await
                    .unwrap()
                    .unwrap()
                    .text,
                "source survives cancellation"
            );
            base.close().await.unwrap();
        }
    }

    #[test]
    fn model_policies_resolve_aliases_and_only_restrict_explicit_overrides() {
        let unrestricted = AcpHarnesses::default()
            .with_model_policies(BTreeMap::from([(
                BUILTIN_HARNESS.into(),
                SubagentHarnessPolicy {
                    models: BTreeMap::from([("review".into(), "provider:model-a".into())]),
                    allow_model_overrides: None,
                },
            )]))
            .unwrap();
        assert_eq!(
            unrestricted
                .resolve_model(BUILTIN_HARNESS, "review")
                .unwrap(),
            "provider:model-a"
        );
        assert_eq!(
            unrestricted
                .resolve_model(BUILTIN_HARNESS, "provider:model-b")
                .unwrap(),
            "provider:model-b"
        );

        let disabled = AcpHarnesses::default()
            .with_model_policies(BTreeMap::from([(
                BUILTIN_HARNESS.into(),
                SubagentHarnessPolicy {
                    models: BTreeMap::new(),
                    allow_model_overrides: Some(Vec::new()),
                },
            )]))
            .unwrap();
        assert!(
            disabled
                .resolve_model(BUILTIN_HARNESS, "provider:model-a")
                .unwrap_err()
                .contains("is not allowed")
        );
    }

    mod forwards_subagent_events {
        use super::*;

        #[tokio::test]
        async fn preserves_nested_roster_event_lines_exactly() {
            let event = crate::events::RuntimeEvent::SubagentStateChanged {
                id: "s-child".into(),
                name: "Scout".into(),
                status: crate::events::SubagentStatus::Working,
                outcome: None,
                generation: 2,
                task: "inspect".into(),
                parent_id: Some("s-parent".into()),
                parent_name: Some("Pip".into()),
                harness: BUILTIN_HARNESS.into(),
                model: Some("model".into()),
                created_at_unix_ms: 10,
                generation_started_at_unix_ms: 20,
                generation_finished_at_unix_ms: None,
            };
            let line = format!(
                "{}{}",
                crate::events::EVENT_MARKER,
                serde_json::to_string(&event).unwrap()
            );
            let input = format!("{line}\n");
            let mut output = Vec::new();
            forward_stderr(input.as_bytes(), "acp.kit", Some("s-owner"), |item| {
                output.push(item)
            })
            .await;

            assert!(matches!(&output[0], ForwardedStderr::RuntimeLine(actual) if actual == &line));
            assert!(matches!(
                &output[1],
                ForwardedStderr::Cleanup(crate::events::RuntimeEvent::SubagentDescendantsRemoved { ancestor_id })
                    if ancestor_id == "s-owner"
            ));
        }
    }

    mod removes_descendants_on_exit {
        use super::*;

        fn cleanup_count(output: &[ForwardedStderr]) -> usize {
            output
                .iter()
                .filter(|item| matches!(item, ForwardedStderr::Cleanup(_)))
                .count()
        }

        #[tokio::test]
        async fn abrupt_child_exit_forwards_exact_roster_lines_and_cleans_descendants_once() {
            let nested = crate::events::RuntimeEvent::SubagentStateChanged {
                id: "s-nested".into(),
                name: "Scout".into(),
                status: crate::events::SubagentStatus::Working,
                outcome: None,
                generation: 3,
                task: "inspect".into(),
                parent_id: Some("s-owner".into()),
                parent_name: Some("Pip".into()),
                harness: BUILTIN_HARNESS.into(),
                model: None,
                created_at_unix_ms: 10,
                generation_started_at_unix_ms: 20,
                generation_finished_at_unix_ms: None,
            };
            let nested_cleanup = crate::events::RuntimeEvent::SubagentDescendantsRemoved {
                ancestor_id: "s-nested".into(),
            };
            let lines = [nested, nested_cleanup]
                .map(|event| {
                    format!(
                        "{}{}",
                        crate::events::EVENT_MARKER,
                        serde_json::to_string(&event).unwrap()
                    )
                })
                .to_vec();
            let payload = format!("{}\n", lines.join("\n"));
            let mut child = Command::new("python3")
                .arg("-c")
                .arg("import os; os.write(2, os.environ['PAYLOAD'].encode()); os._exit(23)")
                .env("PAYLOAD", payload)
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let stderr = child.stderr.take().unwrap();
            let mut output = Vec::new();

            forward_stderr(stderr, "acp.kit", Some("s-owner"), |item| output.push(item)).await;
            let status = child.wait().await.unwrap();

            assert_eq!(status.code(), Some(23));
            let forwarded = output
                .iter()
                .filter_map(|item| match item {
                    ForwardedStderr::RuntimeLine(line) => Some(line.clone()),
                    ForwardedStderr::Diagnostic(_) | ForwardedStderr::Cleanup(_) => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(forwarded, lines);
            assert_eq!(cleanup_count(&output), 1);
            assert!(matches!(
                output.last(),
                Some(ForwardedStderr::Cleanup(
                    crate::events::RuntimeEvent::SubagentDescendantsRemoved { ancestor_id }
                )) if ancestor_id == "s-owner"
            ));
            assert!(!output.iter().any(|item| match item {
                ForwardedStderr::RuntimeLine(line) => matches!(
                    crate::events::parse(line),
                    Some(crate::events::RuntimeEvent::SubagentStateChanged { id, .. })
                        if id == "s-owner"
                ),
                ForwardedStderr::Cleanup(crate::events::RuntimeEvent::SubagentStateChanged {
                    id,
                    ..
                }) => id == "s-owner",
                ForwardedStderr::Diagnostic(_) | ForwardedStderr::Cleanup(_) => false,
            }));
        }

        #[tokio::test]
        async fn emits_cleanup_once_on_normal_eof() {
            let mut output = Vec::new();
            forward_stderr(&b""[..], "acp.kit", Some("s-owner"), |item| {
                output.push(item)
            })
            .await;
            assert_eq!(cleanup_count(&output), 1);
        }

        #[tokio::test]
        async fn emits_cleanup_once_on_abrupt_stream_error() {
            let mut output = Vec::new();
            forward_stderr(&b"\xff"[..], "acp.kit", Some("s-owner"), |item| {
                output.push(item)
            })
            .await;
            assert_eq!(cleanup_count(&output), 1);
        }
    }

    mod subagent_parent_context {
        use super::*;

        fn config(parent_id: Option<&str>, parent_name: Option<&str>) -> ChildConfig {
            ChildConfig {
                root: PathBuf::from("/tmp"),
                model: "model".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                configured_mcp_config: None,
                configured_mcp_config_inherited: false,
                legacy_mcp_config: false,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses: AcpHarnesses::default(),
                default_harness: BUILTIN_HARNESS.into(),
                parent_id: parent_id.map(str::to_owned),
                parent_name: parent_name.map(str::to_owned),
            }
        }

        fn args(command: &Command) -> Vec<String> {
            command
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        }

        #[test]
        fn kit_child_receives_unicode_immediate_parent_context() {
            let config = config(Some("s-parent"), Some("偵察 🦀"));
            let command = config
                .harnesses
                .spawn(BUILTIN_HARNESS, &config, Some(("session", false)), 1)
                .unwrap();
            let args = args(&command);
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--subagent-parent-id", "s-parent"])
            );
            assert!(args.contains(&"--subagent-parent-name=偵察 🦀".into()));
        }

        #[test]
        fn kit_child_parent_name_is_safe_when_it_starts_with_a_hyphen() {
            let config = config(Some("s-parent"), Some("--reviewer"));
            let command = config
                .harnesses
                .spawn(BUILTIN_HARNESS, &config, Some(("session", false)), 1)
                .unwrap();

            assert!(args(&command).contains(&"--subagent-parent-name=--reviewer".into()));
        }

        #[test]
        fn recursive_launch_replaces_inherited_parent_context() {
            let config = config(Some("s-grandparent"), Some("Pip"))
                .with_parent_context("s-parent".into(), "Scout".into());
            assert_eq!(config.parent_id.as_deref(), Some("s-parent"));
            assert_eq!(config.parent_name.as_deref(), Some("Scout"));
        }

        #[test]
        fn top_level_and_generic_acp_commands_do_not_receive_parent_arguments() {
            let top_level = config(None, None);
            let top_level_args = args(
                &top_level
                    .harnesses
                    .spawn(BUILTIN_HARNESS, &top_level, Some(("session", false)), 1)
                    .unwrap(),
            );
            assert!(
                !top_level_args
                    .iter()
                    .any(|arg| arg.starts_with("--subagent-parent-"))
            );

            let harnesses = AcpHarnesses::new(BTreeMap::from([(
                "generic".into(),
                AcpHarnessProfile {
                    command: "generic-acp".into(),
                    args: Vec::new(),
                    permissions: AcpPermissionPolicy::Deny,
                },
            )]))
            .unwrap();
            let mut generic = config(Some("s-parent"), Some("Scout"));
            generic.harnesses = harnesses.clone();
            let generic_args = args(&harnesses.spawn("acp.generic", &generic, None, 1).unwrap());
            assert!(
                !generic_args
                    .iter()
                    .any(|arg| arg.starts_with("--subagent-parent-"))
            );
        }
    }

    #[test]
    fn deny_policy_selects_only_rejection_options() {
        let options = [
            PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowAlways),
            PermissionOption::new("once", "Reject once", PermissionOptionKind::RejectOnce),
            PermissionOption::new(
                "always",
                "Reject always",
                PermissionOptionKind::RejectAlways,
            ),
        ];
        assert_eq!(
            permission_outcome(AcpPermissionPolicy::Deny, &options),
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("always"))
        );
        assert_eq!(
            permission_outcome(AcpPermissionPolicy::Cancel, &options),
            RequestPermissionOutcome::Cancelled
        );
        assert_eq!(
            permission_outcome(AcpPermissionPolicy::Deny, &options[..2]),
            RequestPermissionOutcome::Cancelled
        );
        assert_eq!(
            permission_outcome(AcpPermissionPolicy::Deny, &options[..3]),
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("once"))
        );
    }
}
