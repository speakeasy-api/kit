//! Generic parent-owned ACP subprocesses used for nested agents.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{
    events::{
        DiagnosticOperation, DiagnosticScope, offer_diagnostic_ownership, ownership_supported,
        set_diagnostic_operation,
    },
    tools::mcp::CredentialStorage,
};

/// Bound retained provenance per process, without evicting issued identities.
const MAX_DIAGNOSTIC_OPERATIONS: usize = 4096;

/// Immutable producer ownership, separate from request-lifetime output routes.
/// Both the process actor and stderr drain retain this registry. Entries are
/// never removed on response, cancellation, or session close.
struct DiagnosticOperationRoutes {
    operations: Mutex<(u64, HashMap<DiagnosticOperation, DiagnosticScope>)>,
    negotiated: AtomicBool,
}

impl DiagnosticOperationRoutes {
    fn new() -> Self {
        Self {
            operations: Mutex::new((0, HashMap::new())),
            negotiated: AtomicBool::new(false),
        }
    }

    fn register(&self) -> Result<DiagnosticOperation, ChildError> {
        let scope = DiagnosticScope::capture();
        let mut routes = self
            .operations
            .lock()
            .map_err(|_| ChildError::Failed("diagnostic operation registry unavailable".into()))?;
        if routes.1.len() >= MAX_DIAGNOSTIC_OPERATIONS {
            return Err(ChildError::Failed(format!(
                "nested diagnostic operation capacity exhausted ({MAX_DIAGNOSTIC_OPERATIONS}); close this subagent and start a new child process",
            )));
        }
        routes.0 = routes
            .0
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("diagnostic operation sequence exhausted; close this subagent and start a new child process".into()))?;
        let operation = DiagnosticOperation::new(format!("operation-{}", routes.0))
            .expect("generated diagnostic operation is valid");
        routes.1.insert(operation.clone(), scope);
        Ok(operation)
    }

    fn register_negotiated(&self) -> Result<Option<DiagnosticOperation>, ChildError> {
        if !self.negotiated.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.register().map(Some)
    }

    fn resolve(&self, operation: &DiagnosticOperation) -> Option<DiagnosticScope> {
        self.operations.lock().ok()?.1.get(operation).cloned()
    }
}

fn owned_close(
    session_id: SessionId,
    operation: Option<&DiagnosticOperation>,
) -> CloseSessionRequest {
    let mut request = CloseSessionRequest::new(session_id);
    if let Some(operation) = operation {
        set_diagnostic_operation(&mut request.meta, operation);
    }
    request
}

fn owned_model(
    session_id: SessionId,
    model: &str,
    operation: Option<&DiagnosticOperation>,
) -> SetSessionConfigOptionRequest {
    let mut request = SetSessionConfigOptionRequest::new(session_id, "model", model);
    if let Some(operation) = operation {
        set_diagnostic_operation(&mut request.meta, operation);
    }
    request
}

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
    operation: Option<DiagnosticOperation>,
    session_id: SessionId,
    text: String,
    cancellation: TurnCancellation,
    reply: oneshot::Sender<Result<ChildOutput, ChildError>>,
}
struct Fork {
    serial: tokio::sync::OwnedMutexGuard<()>,
    operation: Option<DiagnosticOperation>,
    session_id: SessionId,
    model: Option<String>,
    parent: Option<(String, String)>,
    cancellation: TurnCancellation,
    reply: oneshot::Sender<Result<SessionId, ChildError>>,
}
struct Close {
    operation: Option<DiagnosticOperation>,
    session_id: SessionId,
    reply: oneshot::Sender<Result<(), ChildError>>,
}
enum Request {
    Prompt(Prompt),
    Fork(Fork),
    Close(Close),
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
    diagnostics: Arc<DiagnosticOperationRoutes>,
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
        if cancellation.is_cancelled() {
            return Err(ChildError::Cancelled);
        }
        let context = config.harnesses.launch_context(&harness);
        let actor_context = context.clone();
        let diagnostics = Arc::new(DiagnosticOperationRoutes::new());
        // Capture before spawn, but retain a token only after explicit agreement
        // and before session/new dispatch. Legacy children need no registrations.
        let admission = DiagnosticScope::capture();
        let actor_diagnostics = Arc::clone(&diagnostics);
        let (tx, mut rx) = mpsc::channel(1);
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = watch::channel(false);
        let actor_tx = tx.clone();
        let mut task = tokio::spawn(crate::events::inherit_diagnostics(async move {
            let result = run(
                RunConfig {
                    config,
                    harness,
                    persisted,
                    model,
                    depth,
                    context: actor_context,
                    diagnostics: actor_diagnostics,
                    admission,
                },
                &mut rx,
                ready_tx,
                closed_tx.clone(),
            )
            .await;
            let _ = closed_tx.send(true);
            result
        }));
        let result = tokio::select! {
            ready = &mut ready_rx => match ready {
                Ok(Ok(ready)) => Ok(ready),
                Ok(Err(error)) => Err(ChildError::Failed(error)),
                Err(_) => return Err(ChildError::Failed(match task.await {
                    Ok(Ok(())) => "nested agent exited during startup".into(),
                    Ok(Err(error)) => error,
                    Err(error) => format!("nested agent startup actor failed: {error}"),
                })),
            },
            () = cancellation.cancelled() => Err(ChildError::Cancelled),
            () = tokio::time::sleep(HANDSHAKE) => Err(ChildError::Failed(context.error(
                "handshake timeout",
                format!("no response within {} seconds", HANDSHAKE.as_secs()),
            ))),
            joined = &mut task => return Err(ChildError::Failed(match joined { Ok(Ok(())) => "nested agent exited during startup".into(), Ok(Err(e)) => e, Err(e) => format!("nested agent startup actor failed: {e}") })),
        };
        match result {
            Ok(ready) => Ok(Self {
                diagnostics,
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
            // Explicit close belongs to its caller, which may be a new turn
            // using a reused or native-forked child. Deferred cleanup callers
            // retain their originating scope across their own spawn boundary.
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
        let permit = self.tx.reserve().await.map_err(|_| {
            ChildError::TerminalFailed("nested agent process is no longer running".into())
        })?;
        // Exhaustion must never strand a live process or native-fork sibling.
        // Omit ownership rather than borrowing an issued token or a current route.
        let operation = self
            .diagnostics
            .register_negotiated()
            .unwrap_or_else(|error| {
                eprintln!("ACP harness closing without diagnostic ownership: {error}");
                None
            });
        let (reply, response) = oneshot::channel();
        permit.send(Request::Close(Close {
            operation,
            session_id: self.session_id.clone(),
            reply,
        }));
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
        let diagnostics = DiagnosticScope::capture();
        let serial = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
            serial = self.serial.clone().lock_owned() => serial,
        };
        if !self.supports_native_fork() {
            return Err(ChildError::Failed(
                "ACP harness does not support session/fork".into(),
            ));
        }
        let descendant_parent = parent.as_ref().map(|(id, _)| id.clone());
        let permit = self.request_permit(cancellation).await?;
        let operation = diagnostics.sync_scope(|| self.diagnostics.register_negotiated())?;
        let (reply, response) = oneshot::channel();
        permit.send(Request::Fork(Fork {
            serial,
            operation,
            session_id: self.session_id.clone(),
            model: model.map(str::to_owned),
            parent,
            cancellation: cancellation.clone(),
            reply,
        }));
        let session_id = response.await.map_err(|_| {
            ChildError::TerminalFailed("nested agent process exited without a fork response".into())
        })??;
        Ok(Self {
            diagnostics: Arc::clone(&self.diagnostics),
            tx: self.tx.clone(),
            session_id,
            capabilities: self.capabilities.clone(),
            serial: Arc::new(tokio::sync::Mutex::new(())),
            closed: self.closed.clone(),
            descendant_parent,
        })
    }

    async fn request_permit(
        &self,
        cancellation: &TurnCancellation,
    ) -> Result<mpsc::Permit<'_, Request>, ChildError> {
        if self.is_closed() {
            return Err(ChildError::TerminalFailed(
                "nested agent process is no longer running".into(),
            ));
        }
        let permit = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
            permit = self.tx.reserve() => permit.map_err(|_| {
                ChildError::TerminalFailed("nested agent process is no longer running".into())
            })?,
        };
        if cancellation.is_cancelled() {
            return Err(ChildError::Cancelled);
        }
        if self.is_closed() {
            return Err(ChildError::TerminalFailed(
                "nested agent process is no longer running".into(),
            ));
        }
        Ok(permit)
    }

    pub async fn prompt(
        &self,
        text: String,
        cancellation: TurnCancellation,
    ) -> Result<ChildOutput, ChildError> {
        let diagnostics = DiagnosticScope::capture();
        let serial = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
            serial = self.serial.clone().lock_owned() => serial,
        };
        let permit = self.request_permit(&cancellation).await?;
        let operation = diagnostics.sync_scope(|| self.diagnostics.register_negotiated())?;
        let (reply, response) = oneshot::channel();
        permit.send(Request::Prompt(Prompt {
            serial,
            operation,
            session_id: self.session_id.clone(),
            text,
            cancellation: cancellation.clone(),
            reply,
        }));
        response.await.map_err(|_| {
            ChildError::TerminalFailed("nested agent process exited without a response".into())
        })?
    }
}

struct RunConfig {
    diagnostics: Arc<DiagnosticOperationRoutes>,
    admission: DiagnosticScope,
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
        diagnostics,
        admission,
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
    let descendant_parent = config.parent_id.clone();
    let (negotiation_tx, negotiation_rx) = watch::channel(None);
    let _stderr_task = spawn_forward_stderr(
        stderr,
        harness.clone(),
        config.parent_id.clone(),
        Some((Arc::clone(&diagnostics), negotiation_rx)),
    );
    // Retain registrations until this actor exits, even if stderr closes first.
    let connection_diagnostics = Arc::clone(&diagnostics);
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
            let mut initialize = agentkit_acp::InitializeRequest::new(ProtocolVersion::V1);
            offer_diagnostic_ownership(&mut initialize.meta);
            let initialized = connection.send_request(initialize).block_task().await?;
            let negotiated = ownership_supported(initialized.meta.as_ref());
            connection_diagnostics.negotiated.store(negotiated, Ordering::Release);
            let _ = negotiation_tx.send(Some(negotiated));
            let admission = admission.sync_scope(|| connection_diagnostics.register_negotiated())
                .map_err(|error| agent_client_protocol::util::internal_error(error.to_string()))?;
            let capabilities = initialized.agent_capabilities;
            let supports_close = capabilities.session_capabilities.close.is_some();
            let mut request = agentkit_acp::NewSessionRequest::new(root.clone());
            if let Some(operation) = &admission {
                set_diagnostic_operation(&mut request.meta, operation);
            }
            let session = connection.send_request(request).block_task().await?;
            if let Some(model) = model {
                let selectable = session.config_options.as_deref().unwrap_or_default().iter().any(|option| {
                    option.id.to_string() == "model" && matches!(option.kind, SessionConfigKind::Select(_))
                });
                if !selectable {
                    let error = format!("ACP harness {harness:?} does not advertise a selectable model session option");
                    let _ = ready.send(Err(error));
                    return std::future::pending().await;
                }
                if let Err(error) = connection.send_request(owned_model(session.session_id.clone(), model.as_str(), admission.as_ref())).block_task().await {
                    let error = format!("ACP harness {harness:?} rejected model selection {model:?}: {error}");
                    let _ = ready.send(Err(error));
                    return std::future::pending().await;
                }
            }
            let sessions = Arc::new(Mutex::new(vec![(session.session_id.clone(), admission.clone())]));
            let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
            let mut tasks = JoinSet::new();
            ready_flag.store(true, Ordering::Release);
            let _ = ready.send(Ok(Ready {
                session_id: session.session_id,
                capabilities,
                descendant_parent,
            }));
            loop {
                let request = tokio::select! {
                    request = rx.recv() => match request { Some(request) => request, None => break },
                    Some(()) = fatal_rx.recv() => return Err(agent_client_protocol::Error::internal_error()),
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
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
                            if let Some(operation) = &fork.operation {
                                set_diagnostic_operation(&mut request.meta, operation);
                            }
                            let mut request = Box::pin(connection.send_request(request).block_task());
                            let result = tokio::select! {
                                result = &mut request => match result {
                                    Ok(response) => {
                                        let session_id = response.session_id;
                                        if let Ok(mut sessions) = sessions.lock() {
                                            sessions.push((session_id.clone(), fork.operation.clone()));
                                        }
                                        if let Some(model) = fork.model {
                                            let selected = match tokio::time::timeout(
                                                HANDSHAKE,
                                                connection
                                                    .send_request(owned_model(
                                                        session_id.clone(), model.as_str(),
                                                        fork.operation.as_ref(),
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
                                                    .send_request(owned_close(session_id.clone(), fork.operation.as_ref()))
                                                    .block_task();
                                                if tokio::time::timeout(CANCEL_SETTLE, close)
                                                    .await
                                                    .is_ok_and(|result| result.is_ok())
                                                    && let Ok(mut sessions) = sessions.lock()
                                                {
                                                    sessions.retain(|(id, _)| id != &session_id);
                                                }
                                            }
                                            selected
                                        } else {
                                            Ok(session_id)
                                        }
                                    }
                                    Err(error) => Err(ChildError::Failed(error.to_string())),
                                },
                                () = fork.cancellation.cancelled() => {
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
                                                    .send_request(owned_close(session_id.clone(), fork.operation.as_ref()))
                                                    .block_task(),
                                            )
                                            .await
                                            .is_ok_and(|result| result.is_ok());
                                        if !closed
                                            && let Ok(mut sessions) = cleanup_sessions.lock()
                                        {
                                            sessions.push((session_id, fork.operation.clone()));
                                        }
                                    });
                                    Err(ChildError::Cancelled)
                                },
                                () = tokio::time::sleep(HANDSHAKE) => {
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
                                                    .send_request(owned_close(session_id.clone(), fork.operation.as_ref()))
                                                    .block_task(),
                                            )
                                            .await
                                            .is_ok_and(|result| result.is_ok());
                                        if !closed
                                            && let Ok(mut sessions) = cleanup_sessions.lock()
                                        {
                                            sessions.push((session_id, fork.operation.clone()));
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
                                .send_request(owned_close(close.session_id.clone(), close.operation.as_ref()))
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
                                sessions.retain(|(id, _)| id != &close.session_id);
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
                            let mut request = agentkit_acp::PromptRequest::new(
                                session_id.clone(), vec![ContentBlock::Text(agentkit_acp::TextContent::new(prompt.text))],
                            );
                            if let Some(operation) = &prompt.operation {
                                set_diagnostic_operation(&mut request.meta, operation);
                            }
                            let request = connection.send_request(request).block_task();
                            tokio::pin!(request);
                            let (response, cancelled) = tokio::select! {
                                biased;
                                result = &mut request => (result.map_err(|error| error.to_string()), false),
                                () = prompt.cancellation.cancelled() => {
                                    let mut cancel = CancelNotification::new(session_id.clone());
                                    if let Some(operation) = &prompt.operation {
                                        set_diagnostic_operation(&mut cancel.meta, operation);
                                    }
                                    let _ = connection.send_notification(cancel);
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
                for (session_id, operation) in session_ids {
                    let close = connection
                        .send_request(owned_close(session_id, operation.as_ref()))
                        .block_task();
                    if let Ok(result) = tokio::time::timeout(CANCEL_SETTLE, close).await {
                        result?;
                    }
                }
            }
            Ok(())
        });
    tokio::pin!(connected);
    let connected = tokio::select! {
        result = &mut connected => result,
        status = child.wait() => {
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

// Retain at most this many bytes while the child negotiates ownership. Draining
// must continue after the limit so a noisy child can still finish initialization.
const PRE_NEGOTIATION_STDERR_LIMIT: usize = 256 * 1024;

#[derive(Default)]
struct PendingStderr {
    bytes: Vec<u8>,
    overflowed: bool,
    discard_partial: bool,
}

impl PendingStderr {
    // Returns true exactly once, when the retention budget is first exceeded.
    fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        if self.overflowed {
            self.discard_partial = bytes.last() != Some(&b'\n');
            return false;
        }
        if self.bytes.capacity() == 0 {
            self.bytes.reserve_exact(PRE_NEGOTIATION_STDERR_LIMIT);
        }
        let remaining = PRE_NEGOTIATION_STDERR_LIMIT - self.bytes.len();
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if bytes.len() <= remaining {
            return false;
        }
        self.overflowed = true;
        // Keep only whole lines. Discard everything else until negotiation ends,
        // including the rest of this partial line even if it crosses the ack.
        let complete = self
            .bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        self.bytes.truncate(complete);
        self.discard_partial = bytes.last() != Some(&b'\n');
        true
    }
}

// Only a newline restores a safe line boundary. EOF or an error must not let
// a later read reinterpret an unconsumed suffix as a fresh runtime event.
async fn discard_stderr_line(stderr: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> bool {
    loop {
        let Ok(bytes) = stderr.fill_buf().await else {
            return false;
        };
        if bytes.is_empty() {
            return false;
        }
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(bytes.len(), |position| position + 1);
        stderr.consume(count);
        if newline.is_some() {
            return true;
        }
    }
}

/// Translate only explicitly negotiated, known operation tokens. Legacy children
/// retain immutable launch provenance; neither reader timing nor child epochs
/// authorize retagging their diagnostics after parent reactivation.
fn spawn_forward_stderr(
    stderr: impl AsyncRead + Unpin + Send + 'static,
    label: String,
    ancestor_id: Option<String>,
    ownership: Option<(
        Arc<DiagnosticOperationRoutes>,
        watch::Receiver<Option<bool>>,
    )>,
) -> tokio::task::JoinHandle<()> {
    let launch = DiagnosticScope::capture();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut pending = PendingStderr::default();
        let routes = if let Some((routes, mut negotiation)) = ownership {
            // Drain while initialization is pending: waiting without reading can
            // fill the stderr pipe and prevent the child from replying at all.
            // Interpret these bytes only after agreement (or failed startup).
            let mut buffer = [0; 8192];
            while negotiation.borrow().is_none() {
                tokio::select! {
                    biased;
                    changed = negotiation.changed() => {
                        if changed.is_err() { break; }
                    }
                    read = stderr.read(&mut buffer) => match read {
                        Ok(0) | Err(_) => {
                            let _ = negotiation.wait_for(Option::is_some).await;
                            break;
                        }
                        Ok(count) => {
                            if pending.push(&buffer[..count]) {
                                eprintln!("[{label}] pre-negotiation stderr exceeded {PRE_NEGOTIATION_STDERR_LIMIT}-byte limit; discarding remaining pre-negotiation output and any incomplete line");
                            }
                        }
                    }
                }
            }
            Some(routes)
        } else {
            None
        };
        let negotiated = routes
            .as_ref()
            .filter(|routes| routes.negotiated.load(Ordering::Acquire));
        let mut stderr = BufReader::new(stderr);
        let resume = !pending.discard_partial || discard_stderr_line(&mut stderr).await;
        forward_stderr(
            pending
                .bytes
                .as_slice()
                .chain(stderr.take(if resume { u64::MAX } else { 0 })),
            &label,
            ancestor_id.as_deref(),
            negotiated.map(Arc::as_ref),
            |output| {
                match output {
                    ForwardedStderr::RuntimeLine(line) => {
                        if let Some(routes) = negotiated {
                            if let Some(envelope) = crate::events::parse_diagnostic(&line)
                                && let Some(scope) = envelope
                                    .operation
                                    .as_ref()
                                    .and_then(|op| routes.resolve(op))
                            {
                                // Restores both the parent's activation and its
                                // upstream operation at every descendant hop.
                                scope.emit(&envelope.event);
                            }
                        } else if let Some(event) = crate::events::parse(&line) {
                            launch.emit(&event);
                        }
                    }
                    ForwardedStderr::Diagnostic(line) => eprintln!("{line}"),
                    ForwardedStderr::Cleanup(event) => launch.emit(&event),
                    ForwardedStderr::OwnedCleanup(event, operation) => {
                        if let Some(scope) =
                            negotiated.and_then(|routes| routes.resolve(&operation))
                        {
                            scope.emit(&event);
                        }
                    }
                }
            },
        )
        .await;
    })
}

#[derive(Debug, PartialEq)]
enum ForwardedStderr {
    RuntimeLine(String),
    Diagnostic(String),
    Cleanup(crate::events::RuntimeEvent),
    OwnedCleanup(crate::events::RuntimeEvent, DiagnosticOperation),
}

async fn forward_stderr(
    stderr: impl AsyncRead + Unpin,
    label: &str,
    ancestor_id: Option<&str>,
    negotiated: Option<&DiagnosticOperationRoutes>,
    mut output: impl FnMut(ForwardedStderr),
) {
    // Legacy cleanup keeps its launch ancestor. Negotiated cleanup is derived
    // solely from accepted roster provenance, not the most recent invocation.
    let mut ancestors = if negotiated.is_none() {
        ancestor_id
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    let mut owned_ancestors =
        Vec::<(DiagnosticScope, DiagnosticOperation, BTreeSet<String>)>::new();
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let envelope = if negotiated.is_some() {
            crate::events::parse_diagnostic(&line)
        } else {
            crate::events::parse(&line).map(|event| crate::events::DiagnosticEvent {
                event,
                activation: None,
                operation: None,
            })
        };
        if let Some(envelope) = envelope
            && envelope.event.forward_from_child()
        {
            if let Some(routes) = negotiated {
                let Some(operation) = envelope.operation else {
                    continue;
                };
                let Some(scope) = routes.resolve(&operation) else {
                    // Missing, malformed, or unknown ownership never falls back.
                    continue;
                };
                let index = owned_ancestors
                    .iter()
                    .position(|(owner, _, _)| owner == &scope)
                    .unwrap_or_else(|| {
                        owned_ancestors.push((scope, operation, BTreeSet::new()));
                        owned_ancestors.len() - 1
                    });
                let ancestors = &mut owned_ancestors[index].2;
                match &envelope.event {
                    crate::events::RuntimeEvent::SubagentStateChanged {
                        parent_id: Some(parent_id),
                        ..
                    } => {
                        ancestors.insert(parent_id.clone());
                    }
                    crate::events::RuntimeEvent::SubagentDescendantsRemoved { ancestor_id } => {
                        ancestors.remove(ancestor_id);
                    }
                    _ => {}
                }
            } else if let crate::events::RuntimeEvent::SubagentStateChanged {
                parent_id: Some(parent_id),
                ..
            } = &envelope.event
            {
                ancestors.insert(parent_id.clone());
            }
            output(ForwardedStderr::RuntimeLine(line));
        } else if let Some(line) = harness_diagnostic(label, &line) {
            // StorageStatus and ordinary external stderr retain existing global
            // diagnostic behavior, independent of operation negotiation.
            output(ForwardedStderr::Diagnostic(line));
        }
    }
    for ancestor_id in ancestors {
        output(ForwardedStderr::Cleanup(
            crate::events::RuntimeEvent::SubagentDescendantsRemoved { ancestor_id },
        ));
    }
    for (_, operation, ancestors) in owned_ancestors {
        for ancestor_id in ancestors {
            output(ForwardedStderr::OwnedCleanup(
                crate::events::RuntimeEvent::SubagentDescendantsRemoved { ancestor_id },
                operation.clone(),
            ));
        }
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
                    diagnostics: Arc::new(DiagnosticOperationRoutes::new()),
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
                diagnostics: Arc::new(DiagnosticOperationRoutes::new()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn update(value: Value) -> SessionUpdate {
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_release_child_request_serialization() {
        for fork in [false, true] {
            let (tx, mut rx) = mpsc::channel(1);
            let mut capabilities = agentkit_acp::AgentCapabilities::default();
            capabilities.session_capabilities.fork = Some(Default::default());
            let child = ChildSession {
                tx,
                session_id: "test".into(),
                capabilities,
                serial: Arc::new(tokio::sync::Mutex::new(())),
                closed: watch::channel(false).1,
                descendant_parent: None,
                diagnostics: Arc::new(DiagnosticOperationRoutes::new()),
            };
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

    /// Real stdio requests capture producer tokens; a delayed producer is released
    /// only by the following request, after the old prompt has already replied.
    #[test]
    fn negotiated_stdio_ownership_survives_reuse_and_native_forks() {
        use crate::events::{self, RuntimeEvent};
        const PROBE: &str = "KIT_TEST_NEGOTIATED_CHILD_PROBE";
        if std::env::var_os(PROBE).is_some() {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2).enable_all().build().unwrap().block_on(async {
                    let root = tempfile::tempdir().unwrap();
                    let script = root.path().join("ownership.py");
                    std::fs::write(&script, r#"
import json, sys, threading
mode = sys.argv[1]
marker = sys.argv[2]
lock = threading.Lock()
next_session = 0
delayed = None
barrier = threading.Barrier(2)
def send(value):
    with lock:
        print(json.dumps(value), flush=True)
def response(request, result):
    send({'jsonrpc': '2.0', 'id': request['id'], 'result': result})
def emit(operation, label):
    event = {'event': 'child_started', 'call': label, 'tool': label, 'summary': label, 'at': 1}
    if operation is not None: event['operation'] = operation
    with lock:
        print(marker + json.dumps(event), file=sys.stderr, flush=True)
def prompt(request):
    global delayed
    params = request['params']
    operation = params.get('_meta', {}).get('kitDiagnosticOperation')
    label = params['prompt'][0]['text']
    if label == 'old':
        delayed = (operation, label)
    else:
        if label == 'new' and delayed is not None:
            emit(*delayed)
        if label.startswith('sibling'): barrier.wait(timeout=5)
        emit(operation, label)
        if label == 'new':
            emit('unknown-token', 'unknown')
            emit(None, 'missing')
            emit(42, 'malformed')
    response(request, {'stopReason': 'end_turn'})
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    params = request.get('params', {})
    if method == 'initialize':
        assert params['_meta']['kitDiagnosticOwnership'] == {'version': 1, 'transport': 'stderr'}
        result = {'protocolVersion': 1, 'agentCapabilities': {'sessionCapabilities': {'fork': {}, 'close': {}}}}
        if mode != 'absent':
            result['_meta'] = {'kitDiagnosticOwnership': {'version': 1 if mode == 'supported' else 99, 'transport': 'stderr'}}
        if mode == 'supported':
            for _ in range(2048): emit(None, 'before-agreement')
        response(request, result)
        continue
    operation = params.get('_meta', {}).get('kitDiagnosticOperation')
    assert (operation is not None) == (mode == 'supported'), (method, params)
    if method in ('session/new', 'session/fork'):
        if method == 'session/fork':
            assert params['_meta']['kit.subagent.parent_id'].startswith('fork-')
            assert params['_meta']['kit.subagent.parent_name'] == 'Fork owner'
        next_session += 1
        emit(operation, method + '-' + str(next_session))
        response(request, {'sessionId': str(next_session)})
    elif method == 'session/prompt':
        threading.Thread(target=prompt, args=(request,), daemon=True).start()
    elif method == 'session/close':
        emit(operation, 'close-' + params['sessionId'])
        response(request, {})
"#).unwrap();
                    for mode in ["supported", "absent", "unknown-version"] {
                        let harnesses = AcpHarnesses::new(BTreeMap::from([("ownership".into(), AcpHarnessProfile {
                            command: "python3".into(),
                            args: vec![script.to_string_lossy().into_owned(), mode.into(), events::EVENT_MARKER.into()],
                            permissions: AcpPermissionPolicy::Deny,
                        })])).unwrap();
                        let config = ChildConfig {
                            root: root.path().to_path_buf(), model: "unused".into(),
                            provider: Default::default(), reasoning_effort: None, openrouter_api_key: None,
                            configured_mcp_config: None, configured_mcp_config_inherited: false,
                            legacy_mcp_config: false, mcp_config: None, credential_storage: Default::default(),
                            telemetry: Default::default(), harnesses, default_harness: "acp.ownership".into(),
                            parent_id: None, parent_name: None,
                        };
                        let base = owned_child_invocation(mode, "launch", ChildSession::start(
                            config, "acp.ownership".into(), None, None, 1, TurnCancellation::default(),
                        )).await.unwrap();
                        assert_eq!(base.diagnostics.negotiated.load(Ordering::Acquire), mode == "supported");
                        owned_child_invocation(mode, "old", base.prompt("old".into(), TurnCancellation::default())).await.unwrap();
                        owned_child_invocation(mode, "new", base.prompt("new".into(), TurnCancellation::default())).await.unwrap();
                        // A→B→A and A→A activate fresh epochs on the same process.
                        owned_child_invocation(mode, "again", base.prompt("again".into(), TurnCancellation::default())).await.unwrap();
                        let first = owned_child_invocation(mode, "fork-first", base.fork(None,
                            Some(("fork-first".into(), "Fork owner".into())), &TurnCancellation::default())).await.unwrap();
                        let second = owned_child_invocation(mode, "fork-second", base.fork(None,
                            Some(("fork-second".into(), "Fork owner".into())), &TurnCancellation::default())).await.unwrap();
                        let (first_result, second_result) = tokio::join!(
                            owned_child_invocation(mode, "sibling-first", first.prompt("sibling-first".into(), TurnCancellation::default())),
                            owned_child_invocation(mode, "sibling-second", second.prompt("sibling-second".into(), TurnCancellation::default())),
                        );
                        first_result.unwrap(); second_result.unwrap();
                        owned_child_invocation(mode, "close-first", first.close()).await.unwrap();
                        owned_child_invocation(mode, "close-second", second.close()).await.unwrap();
                        owned_child_invocation(mode, "close-base", base.close()).await.unwrap();
                        // Every completed request still has its immutable registration.
                        assert_eq!(base.diagnostics.operations.lock().unwrap().1.len(), if mode == "supported" { 11 } else { 0 });
                        let mut closed = base.closed_signal();
                        let registry = Arc::downgrade(&base.diagnostics);
                        drop(first); drop(second); drop(base);
                        tokio::time::timeout(Duration::from_secs(5), closed.wait_for(|closed| *closed)).await.unwrap().unwrap();
                        tokio::time::timeout(Duration::from_secs(5), async {
                            while registry.upgrade().is_some() {
                                tokio::task::yield_now().await;
                            }
                        }).await.expect("process exit and stderr drain release registrations");
                    }
                });
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acp_child::tests::negotiated_stdio_ownership_survives_reuse_and_native_forks",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .env(events::EVENTS_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        let diagnostics = stderr
            .lines()
            .filter_map(events::parse_diagnostic)
            .collect::<Vec<_>>();
        for (call, owner) in [
            ("session/new-1", "launch"),
            ("old", "old"),
            ("new", "new"),
            ("again", "again"),
            ("session/fork-2", "fork-first"),
            ("session/fork-3", "fork-second"),
            ("sibling-first", "sibling-first"),
            ("sibling-second", "sibling-second"),
            ("close-2", "close-first"),
            ("close-3", "close-second"),
            ("close-1", "close-base"),
        ] {
            let envelope = diagnostics.iter().find(|envelope| {
                envelope.operation.as_ref().is_some_and(|op| op.as_str() == format!("supported:{owner}"))
                    && matches!(&envelope.event, RuntimeEvent::ChildStarted { call: actual, .. } if actual == call)
            }).unwrap_or_else(|| panic!("missing owned event {call}: {stderr}"));
            let admission = diagnostics
                .iter()
                .find(|event| {
                    event.operation == envelope.operation
                        && matches!(event.event, RuntimeEvent::SessionStarted { .. })
                })
                .expect("invocation activation marker");
            assert!(envelope.activation.is_some());
            assert_eq!(
                envelope.activation, admission.activation,
                "wrong epoch for {call}"
            );
        }
        let activations = ["old", "new", "again"].map(|label| {
            diagnostics.iter().find(|envelope| {
                envelope.operation.as_ref().is_some_and(|operation| operation.as_str() == format!("supported:{label}"))
                    && matches!(&envelope.event, RuntimeEvent::ChildStarted { call, .. } if call == label)
            }).unwrap().activation.as_ref().unwrap()
        });
        assert_eq!(activations[0].session_id, "supported:A");
        assert_eq!(activations[1].session_id, "supported:B");
        assert_eq!(activations[2].session_id, "supported:A");
        assert!(activations[0].epoch < activations[1].epoch);
        assert!(activations[1].epoch < activations[2].epoch);
        assert!(!diagnostics.iter().any(|envelope| {
            envelope.operation.as_ref().is_some_and(|op| op.as_str().starts_with("supported:"))
                && matches!(&envelope.event, RuntimeEvent::ChildStarted { call, .. } if ["unknown", "missing", "malformed", "before-agreement"].contains(&call.as_str()))
        }));
        for mode in ["absent", "unknown-version"] {
            let forwarded = diagnostics
                .iter()
                .filter(|envelope| {
                    matches!(&envelope.event, RuntimeEvent::ChildStarted { .. })
                        && envelope
                            .operation
                            .as_ref()
                            .is_some_and(|op| op.as_str().starts_with(mode))
                })
                .collect::<Vec<_>>();
            assert!(!forwarded.is_empty());
            assert!(
                forwarded
                    .iter()
                    .all(|envelope| envelope.operation.as_ref().unwrap().as_str()
                        == format!("{mode}:launch"))
            );
        }
    }

    async fn owned_child_invocation<T>(
        mode: &str,
        label: &str,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        let session = format!("{mode}:{}", if label == "new" { "B" } else { "A" });
        DiagnosticScope::with_operation(DiagnosticOperation::new(format!("{mode}:{label}")))
            .scope(async {
                crate::events::activate_diagnostics(&session);
                crate::events::scope_diagnostics(&session, future).await
            })
            .await
    }

    #[test]
    fn real_grandchild_emission_restores_ownership_at_both_boundaries() {
        use crate::events::{self, RuntimeEvent};
        const MODE: &str = "KIT_TEST_GRANDCHILD_OWNERSHIP";
        const TOKEN: &str = "KIT_TEST_GRANDCHILD_TOKEN";
        let mode = std::env::var(MODE).unwrap_or_default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        if mode == "grandchild" {
            runtime.block_on(
                DiagnosticScope::with_operation(DiagnosticOperation::new(
                    std::env::var(TOKEN).unwrap(),
                ))
                .scope(async {
                    events::emit(&RuntimeEvent::ChildStarted {
                        call: "grandchild-producer".into(),
                        tool: "shell".into(),
                        summary: "real producer".into(),
                        at: 1,
                    });
                }),
            );
            return;
        }
        if mode == "child" {
            runtime.block_on(async {
                // One reused intermediate process serves two distinct upstream
                // invocations. Each grandchild has its own connection-local map.
                for upstream in ["operation-1", "operation-2"] {
                    DiagnosticScope::with_operation(DiagnosticOperation::new(upstream.into()))
                        .scope(async {
                            events::activate_diagnostics("child-local");
                            events::scope_diagnostics("child-local", async {
                                let routes = Arc::new(DiagnosticOperationRoutes::new());
                                routes.negotiated.store(true, Ordering::Release);
                                let _unrelated = routes.register().unwrap();
                                let operation = routes.register().unwrap();
                                let mut producer = tokio::process::Command::new(std::env::current_exe().unwrap())
                                    .args(["--exact", "acp_child::tests::real_grandchild_emission_restores_ownership_at_both_boundaries", "--nocapture"])
                                    .env(MODE, "grandchild").env(TOKEN, operation.as_str())
                                    .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
                                let (_ready, ready) = watch::channel(Some(true));
                                let forwarding = spawn_forward_stderr(producer.stderr.take().unwrap(),
                                    "grandchild".into(), None, Some((routes, ready)));
                                assert!(producer.wait().await.unwrap().success());
                                forwarding.await.unwrap();
                            }).await;
                        }).await;
                }
            });
            return;
        }
        let routes = DiagnosticOperationRoutes::new();
        let original = runtime.block_on(async {
            let mut expected = Vec::new();
            for upstream in ["root-old", "root-new"] {
                DiagnosticScope::with_operation(DiagnosticOperation::new(upstream.into()))
                    .scope(async {
                        events::activate_diagnostics("grandchild-parent");
                        events::scope_diagnostics("grandchild-parent", async {
                            expected.push(DiagnosticScope::capture());
                            routes.register().unwrap();
                        })
                        .await;
                    })
                    .await;
            }
            expected
        });
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acp_child::tests::real_grandchild_emission_restores_ownership_at_both_boundaries",
                "--nocapture",
            ])
            .env(MODE, "child")
            .env(events::EVENTS_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut restored = Vec::new();
        // All bytes arrive after parent reactivation. Child-local epochs cannot
        // authorize a parent route; only the second connection's exact map can.
        runtime.block_on(forward_stderr(
            output.stderr.as_slice(),
            "child",
            None,
            Some(&routes),
            |item| {
                if let ForwardedStderr::RuntimeLine(line) = item {
                    let envelope = events::parse_diagnostic(&line).unwrap();
                    assert!(matches!(envelope.event, RuntimeEvent::ChildStarted { .. }));
                    assert_eq!(envelope.activation.unwrap().session_id, "child-local");
                    restored.push(
                        routes
                            .resolve(envelope.operation.as_ref().unwrap())
                            .unwrap(),
                    );
                }
            },
        ));
        assert_eq!(restored, original);
        assert_ne!(restored[0], restored[1]);
    }

    #[tokio::test]
    async fn negotiated_cleanup_uses_only_accepted_scopes_and_deduplicates() {
        use crate::events::{DiagnosticEvent, RuntimeEvent, SubagentStatus};
        use tokio::io::AsyncWriteExt;
        let routes = Arc::new(DiagnosticOperationRoutes::new());
        let scope = DiagnosticScope::with_operation(DiagnosticOperation::new("upstream-a".into()));
        let first = scope.scope(async { routes.register().unwrap() }).await;
        let duplicate = scope.scope(async { routes.register().unwrap() }).await;
        let second = DiagnosticScope::with_operation(DiagnosticOperation::new("upstream-b".into()))
            .scope(async { routes.register().unwrap() })
            .await;
        let roster = RuntimeEvent::SubagentStateChanged {
            id: "descendant".into(),
            name: "Nested".into(),
            status: SubagentStatus::Working,
            outcome: None,
            generation: 1,
            task: "inspect".into(),
            parent_id: Some("shared-ancestor".into()),
            parent_name: None,
            harness: BUILTIN_HARNESS.into(),
            model: None,
            created_at_unix_ms: 1,
            generation_started_at_unix_ms: 1,
            generation_finished_at_unix_ms: None,
        };
        for abrupt in [false, true] {
            let (mut writer, reader) = tokio::io::duplex(64);
            let write_first = first.clone();
            let write_duplicate = duplicate.clone();
            let write_second = second.clone();
            let write_roster = roster.clone();
            let producer = tokio::spawn(async move {
                for operation in [
                    Some(write_first),
                    Some(write_duplicate),
                    Some(write_second),
                    None,
                    DiagnosticOperation::new("unknown".into()),
                ] {
                    let line = format!(
                        "{}{}\n",
                        crate::events::EVENT_MARKER,
                        serde_json::to_string(&DiagnosticEvent {
                            event: write_roster.clone(),
                            activation: None,
                            operation,
                        })
                        .unwrap()
                    );
                    writer.write_all(line.as_bytes()).await.unwrap();
                }
                writer.write_all(b"ordinary stderr\n").await.unwrap();
                let storage = format!(
                    "{}{}\n",
                    crate::events::EVENT_MARKER,
                    serde_json::to_string(&RuntimeEvent::StorageStatus {
                        pending: true,
                        exhausted: false
                    })
                    .unwrap()
                );
                writer.write_all(storage.as_bytes()).await.unwrap();
                if abrupt {
                    writer.write_all(&[0xff]).await.unwrap();
                }
            });
            let mut output = Vec::new();
            forward_stderr(
                reader,
                BUILTIN_HARNESS,
                Some("unrelated-launch"),
                Some(&routes),
                |item| output.push(item),
            )
            .await;
            producer.await.unwrap();
            assert_eq!(
                output
                    .iter()
                    .filter(|item| matches!(item, ForwardedStderr::RuntimeLine(_)))
                    .count(),
                3
            );
            let cleanups = output
                .iter()
                .filter_map(|item| match item {
                    ForwardedStderr::OwnedCleanup(
                        RuntimeEvent::SubagentDescendantsRemoved { ancestor_id },
                        operation,
                    ) => {
                        assert_eq!(ancestor_id, "shared-ancestor");
                        Some(operation.clone())
                    }
                    ForwardedStderr::Cleanup(_) => {
                        panic!("negotiated cleanup used launch provenance")
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(cleanups.len(), 2);
            assert!(cleanups.contains(&first));
            assert!(cleanups.contains(&second));
            assert!(output.iter().any(|item| matches!(item, ForwardedStderr::Diagnostic(line) if line.ends_with("ordinary stderr"))));
            assert!(output.iter().any(|item| matches!(item, ForwardedStderr::Diagnostic(line) if line.contains("storage_status"))));
            // EOF and stream errors do not retire registrations while a process
            // (or a retained logical session) could still own the connection.
            assert!(routes.resolve(&first).is_some());
        }
    }

    #[test]
    fn operation_capacity_bounds_dispatch_without_stranding_close() {
        use crate::events::{self, RuntimeEvent};
        const PROBE: &str = "KIT_TEST_OPERATION_CAPACITY";
        if std::env::var_os(PROBE).is_some() {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let root = tempfile::tempdir().unwrap();
                    let script = root.path().join("capacity.py");
                    std::fs::write(
                        &script,
                        r#"
import json, sys, pathlib
mode, marker, directory = sys.argv[1:]
root = pathlib.Path(directory)
entered = root / 'entered'
entered.unlink(missing_ok=True)
owned = mode != 'legacy'
first = None
cancelled_token = None
pending = None
sessions = 0
prompts = 0
forks = 0
closes = []
def response(request, result):
    print(json.dumps({'jsonrpc':'2.0', 'id':request['id'], 'result':result}), flush=True)
def emit(token, label):
    event = {'event':'child_started', 'call':label, 'tool':'shell', 'summary':label, 'at':1}
    if token is not None: event['operation'] = token
    print(marker + json.dumps(event), file=sys.stderr, flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    params = request.get('params', {})
    token = params.get('_meta', {}).get('kitDiagnosticOperation')
    if method == 'initialize':
        caps = {'close': {}}
        if mode != 'no-fork': caps['fork'] = {}
        result = {'protocolVersion':1, 'agentCapabilities': {'sessionCapabilities':caps}}
        if owned: result['_meta'] = {'kitDiagnosticOwnership':{'version':1,'transport':'stderr'}}
        response(request, result)
    elif method in ['session/new','session/fork']:
        assert (token is not None) == owned
        sessions += 1
        if method == 'session/fork': forks += 1
        response(request, {'sessionId':str(sessions)})
    elif method == 'session/prompt':
        assert (token is not None) == owned
        prompts += 1
        text = params['prompt'][0]['text']
        if first is None: first = token
        if text == 'cancel':
            pending = request
            cancelled_token = token
            entered.write_text('ready')
        else:
            response(request, {'stopReason':'end_turn'})
    elif method == 'session/cancel':
        assert pending is not None and token == cancelled_token
        response(pending, {'stopReason':'cancelled'})
        pending = None
    elif method == 'session/close':
        closes.append(token)
        if len(closes) == 1:
            if first is not None: emit(first, 'old-after-capacity')
            if cancelled_token is not None: emit(cancelled_token, 'cancelled-after-capacity')
            emit(token, 'close-diagnostic')
        response(request, {})
        if len(closes) == sessions:
            (root / 'stats').write_text(json.dumps({'prompts':prompts, 'forks':forks,
                'closes':closes, 'first':first, 'cancelled':cancelled_token}))
            break
"#,
                    )
                    .unwrap();
                    for mode in ["supported", "legacy", "no-fork"] {
                        let harnesses = AcpHarnesses::new(BTreeMap::from([(
                            "capacity".into(),
                            AcpHarnessProfile {
                                command: "python3".into(),
                                args: vec![
                                    script.to_string_lossy().into_owned(),
                                    mode.into(),
                                    events::EVENT_MARKER.into(),
                                    root.path().to_string_lossy().into_owned(),
                                ],
                                permissions: AcpPermissionPolicy::Deny,
                            },
                        )]))
                        .unwrap();
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
                            default_harness: "acp.capacity".into(),
                            parent_id: None,
                            parent_name: None,
                        };
                        let original = DiagnosticScope::with_operation(DiagnosticOperation::new(
                            format!("capacity-original-{mode}"),
                        ));
                        let base = original
                            .scope(ChildSession::start(
                                config.clone(),
                                "acp.capacity".into(),
                                None,
                                None,
                                1,
                                TurnCancellation::default(),
                            ))
                            .await
                            .unwrap();
                        original
                            .scope(base.prompt("first".into(), TurnCancellation::default()))
                            .await
                            .unwrap();
                        let registry = base.diagnostics.clone();
                        if mode == "no-fork" {
                            assert!(matches!(
                                base.fork(None, None, &TurnCancellation::default()).await,
                                Err(ChildError::Failed(_))
                            ));
                            assert_eq!(registry.operations.lock().unwrap().1.len(), 2);
                            base.close().await.unwrap();
                            let mut closed = base.closed_signal();
                            drop(base);
                            tokio::time::timeout(
                                Duration::from_secs(5),
                                closed.wait_for(|closed| *closed),
                            )
                            .await
                            .unwrap()
                            .unwrap();
                            tokio::time::timeout(Duration::from_secs(5), async {
                                while Arc::strong_count(&registry) != 1 {
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .unwrap();
                            continue;
                        }
                        let newer = DiagnosticScope::with_operation(DiagnosticOperation::new(
                            format!("capacity-new-{mode}"),
                        ));
                        let sibling = newer
                            .scope(base.fork(None, None, &TurnCancellation::default()))
                            .await
                            .unwrap();
                        let before_rejection = registry.operations.lock().unwrap().1.len();
                        // Poll the actual prompt while ordinary serial admission is
                        // held, then cancel it. No channel or token is consumed.
                        let serial = base.serial.lock().await;
                        let controller = agentkit_core::CancellationController::new();
                        let mut waiting =
                            Box::pin(base.prompt(
                                "never-dispatched".into(),
                                controller.handle().checkpoint(),
                            ));
                        std::future::poll_fn(|cx| {
                            assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
                            std::task::Poll::Ready(())
                        })
                        .await;
                        controller.interrupt();
                        assert!(matches!(waiting.await, Err(ChildError::Cancelled)));
                        drop(serial);
                        assert_eq!(
                            registry.operations.lock().unwrap().1.len(),
                            before_rejection
                        );
                        // A dispatched cancellation DOES retain its original token.
                        let controller = agentkit_core::CancellationController::new();
                        let cancellation = controller.handle().checkpoint();
                        let child = base.clone();
                        let pending = tokio::spawn(newer.scope(async move {
                            child.prompt("cancel".into(), cancellation).await
                        }));
                        tokio::time::timeout(Duration::from_secs(5), async {
                            while !root.path().join("entered").exists() {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        controller.interrupt();
                        assert!(matches!(pending.await.unwrap(), Err(ChildError::Cancelled)));
                        let rounds = if mode == "supported" {
                            MAX_DIAGNOSTIC_OPERATIONS - 4
                        } else {
                            MAX_DIAGNOSTIC_OPERATIONS + 1
                        };
                        for _ in 0..rounds {
                            newer
                                .scope(base.prompt("fill".into(), TurnCancellation::default()))
                                .await
                                .unwrap();
                        }
                        let expected = if mode == "supported" {
                            MAX_DIAGNOSTIC_OPERATIONS
                        } else {
                            0
                        };
                        assert_eq!(registry.operations.lock().unwrap().1.len(), expected);
                        if mode == "supported" {
                            for child in [&base, &sibling] {
                                let error = child
                                    .prompt("over-capacity".into(), TurnCancellation::default())
                                    .await
                                    .unwrap_err();
                                assert!(error.to_string().contains("capacity exhausted"));
                                assert!(error.to_string().contains("new child process"));
                            }
                            let error =
                                match base.fork(None, None, &TurnCancellation::default()).await {
                                    Err(error) => error,
                                    Ok(_) => panic!("fork dispatched beyond capacity"),
                                };
                            assert!(error.to_string().contains("capacity exhausted"));
                        }
                        // Even with shared native sessions and a full registry,
                        // both closes cross the real transport and are acknowledged.
                        newer.scope(base.close()).await.unwrap();
                        newer.scope(sibling.close()).await.unwrap();
                        let mut closed = base.closed_signal();
                        tokio::time::timeout(
                            Duration::from_secs(5),
                            closed.wait_for(|closed| *closed),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                        assert!(
                            base.prompt("already-exited".into(), TurnCancellation::default())
                                .await
                                .is_err()
                        );
                        assert_eq!(registry.operations.lock().unwrap().1.len(), expected);
                        drop(sibling);
                        drop(base);
                        // The actor and stderr reader must both release their
                        // references: this observes process exit AND stderr drain.
                        tokio::time::timeout(Duration::from_secs(5), async {
                            while Arc::strong_count(&registry) != 1 {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        let stats: Value = serde_json::from_slice(
                            &std::fs::read(root.path().join("stats")).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(stats["prompts"].as_u64().unwrap(), (rounds + 2) as u64);
                        assert_eq!(stats["forks"], 1);
                        assert_eq!(stats["closes"], serde_json::json!([null, null]));
                        if mode == "supported" {
                            let first: DiagnosticOperation =
                                serde_json::from_value(stats["first"].clone()).unwrap();
                            let cancelled: DiagnosticOperation =
                                serde_json::from_value(stats["cancelled"].clone()).unwrap();
                            assert_eq!(registry.resolve(&first), Some(original));
                            assert_eq!(registry.resolve(&cancelled), Some(newer.clone()));
                            // Only a NEW connection resets capacity. Keep the old
                            // full registry alive while the fresh process works.
                            let fresh = ChildSession::start(
                                config,
                                "acp.capacity".into(),
                                None,
                                None,
                                1,
                                TurnCancellation::default(),
                            )
                            .await
                            .unwrap();
                            assert_eq!(fresh.diagnostics.operations.lock().unwrap().1.len(), 1);
                            fresh
                                .prompt("restart".into(), TurnCancellation::default())
                                .await
                                .unwrap();
                            fresh.close().await.unwrap();
                            let mut closed = fresh.closed_signal();
                            drop(fresh);
                            tokio::time::timeout(
                                Duration::from_secs(5),
                                closed.wait_for(|closed| *closed),
                            )
                            .await
                            .unwrap()
                            .unwrap();
                            assert_eq!(
                                registry.operations.lock().unwrap().1.len(),
                                MAX_DIAGNOSTIC_OPERATIONS
                            );
                        } else {
                            assert_eq!(registry.operations.lock().unwrap().0, 0);
                        }
                    }
                });
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acp_child::tests::operation_capacity_bounds_dispatch_without_stranding_close",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .env(events::EVENTS_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        let events = stderr
            .lines()
            .filter_map(events::parse_diagnostic)
            .collect::<Vec<_>>();
        for (label, operation) in [
            ("old-after-capacity", "capacity-original-supported"),
            ("cancelled-after-capacity", "capacity-new-supported"),
        ] {
            assert!(events.iter().any(|event| {
                event.operation.as_ref().is_some_and(|owner| owner.as_str() == operation)
                    && matches!(&event.event, RuntimeEvent::ChildStarted { call, .. } if call == label)
            }), "missing preserved {label}: {stderr}");
        }
        assert!(!events.iter().any(|event| {
            event.operation.as_ref().is_some_and(|owner| owner.as_str().ends_with("-supported"))
                && matches!(&event.event, RuntimeEvent::ChildStarted { call, .. } if call == "close-diagnostic")
        }), "unowned exhausted close borrowed an old/current owner");
        assert!(stderr.contains("closing without diagnostic ownership"));
    }

    #[test]
    fn diagnostic_operation_sequence_exhaustion_never_reuses_a_token() {
        let routes = DiagnosticOperationRoutes::new();
        let first = routes.register().unwrap();
        routes.operations.lock().unwrap().0 = u64::MAX;
        assert!(routes.register().is_err());
        assert!(routes.register().is_err());
        assert!(routes.resolve(&first).is_some());
        assert_eq!(routes.operations.lock().unwrap().1.len(), 1);
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
            forward_stderr(input.as_bytes(), "acp.kit", Some("s-owner"), None, |item| {
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

    #[test]
    fn spawned_stderr_keeps_origin_activation_and_unscoped_is_none() {
        use crate::events::{self, DiagnosticActivation, RuntimeEvent};
        const PROBE: &str = "KIT_TEST_STDERR_ACTIVATION_PROBE";
        if std::env::var_os(PROBE).is_some() {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    use tokio::io::AsyncWriteExt;
                    for route in ["same", "roundtrip", "construction", "unscoped", "current"] {
                        let session = format!("stderr-{route}");
                        events::activate_diagnostics(&session);
                        if route == "current" {
                            events::activate_diagnostics("stderr-current-other");
                            events::activate_diagnostics(&session);
                        }
                        let (mut writer, reader) = tokio::io::duplex(4096);
                        // Also exercise a future constructed before reactivation
                        // but first polled afterward (the construction case).
                        // The parent must reactivate and supply stderr bytes
                        // before awaiting the returned forwarder task.
                        #[allow(clippy::async_yields_async)]
                        let producer = events::scope_diagnostics(&session, async move {
                            spawn_forward_stderr(reader, "acp.kit".into(), Some(route.into()), None)
                        });
                        let (task, delayed) = if route == "construction" {
                            (None, Some(producer))
                        } else if route == "unscoped" {
                            drop(producer);
                            let (new_writer, reader) = tokio::io::duplex(4096);
                            writer = new_writer;
                            (
                                Some(spawn_forward_stderr(
                                    reader,
                                    "acp.kit".into(),
                                    Some(route.into()),
                                    None,
                                )),
                                None,
                            )
                        } else {
                            (Some(producer.await), None)
                        };
                        if route == "roundtrip" {
                            events::activate_diagnostics("stderr-other");
                        }
                        if route != "current" {
                            events::activate_diagnostics(&session);
                        }
                        let task = match task {
                            Some(task) => task,
                            None => delayed.unwrap().await,
                        };
                        // Actual stderr transport gates both forwarded events
                        // and EOF cleanup until the new activation is active.
                        let event = RuntimeEvent::SubagentDescendantsRemoved {
                            ancestor_id: format!("forward-{route}"),
                        };
                        let child_line = events::DiagnosticEvent {
                            operation: None,
                            event,
                            activation: Some(DiagnosticActivation {
                                session_id: "child-process".into(),
                                epoch: 999,
                            }),
                        };
                        writer
                            .write_all(
                                format!(
                                    "{}{}\n",
                                    events::EVENT_MARKER,
                                    serde_json::to_string(&child_line).unwrap()
                                )
                                .as_bytes(),
                            )
                            .await
                            .unwrap();
                        writer.shutdown().await.unwrap();
                        task.await.unwrap();
                    }
                    events::scope_diagnostics("stderr-same", async {
                        events::emit(&RuntimeEvent::StorageStatus {
                            pending: true,
                            exhausted: false,
                        });
                    })
                    .await;
                    events::emit(&RuntimeEvent::CompactionStarted {
                        reason: "unscoped".into(),
                        at: 1,
                    });
                });
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acp_child::tests::spawned_stderr_keeps_origin_activation_and_unscoped_is_none",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .env(events::EVENTS_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        let diagnostics = stderr
            .lines()
            .filter_map(events::parse_diagnostic)
            .collect::<Vec<_>>();
        for route in ["same", "roundtrip", "construction", "unscoped", "current"] {
            let session = format!("stderr-{route}");
            let first = diagnostics
                .iter()
                .find_map(|line| match &line.event {
                    RuntimeEvent::SessionStarted { session_id } if session_id == &session => {
                        line.activation.clone()
                    }
                    _ => None,
                })
                .expect("missing initial activation");
            let expected = if route == "current" {
                diagnostics
                    .iter()
                    .rev()
                    .find_map(|line| match &line.event {
                        RuntimeEvent::SessionStarted { session_id } if session_id == &session => {
                            line.activation.clone()
                        }
                        _ => None,
                    })
                    .unwrap()
            } else {
                first
            };
            for ancestor in [route.to_string(), format!("forward-{route}")] {
                let line = diagnostics.iter().find(|line| matches!(&line.event,
                    RuntimeEvent::SubagentDescendantsRemoved { ancestor_id } if ancestor_id == &ancestor))
                    .expect("missing forwarded event or EOF cleanup");
                assert_eq!(
                    line.activation,
                    (route != "unscoped").then(|| expected.clone())
                );
            }
        }
        for line in diagnostics.iter().filter(|line| {
            matches!(
                line.event,
                RuntimeEvent::StorageStatus { .. } | RuntimeEvent::CompactionStarted { .. }
            )
        }) {
            assert_eq!(line.activation, None);
        }
        assert!(
            diagnostics
                .iter()
                .any(|line| matches!(line.event, RuntimeEvent::StorageStatus { .. }))
        );
        assert!(
            diagnostics
                .iter()
                .any(|line| matches!(line.event, RuntimeEvent::CompactionStarted { .. }))
        );
    }

    mod pre_negotiation_stderr {
        use super::*;

        #[test]
        fn below_budget_retains_partial_lines_and_split_utf8_exactly() {
            let mut pending = PendingStderr::default();
            for bytes in [
                &b"complete\npartial "[..],
                &b"\xe2"[..],
                &b"\x82\xac"[..],
                &b""[..],
            ] {
                assert!(!pending.push(bytes));
            }
            assert_eq!(pending.bytes, "complete\npartial €".as_bytes());
            assert!(!pending.overflowed);
            assert!(!pending.discard_partial);
        }

        #[test]
        fn retention_stays_within_production_budget_and_keeps_whole_prefix_lines() {
            let mut pending = PendingStderr::default();
            assert!(!pending.push(b"kept\n"));
            let fill = vec![b'x'; PRE_NEGOTIATION_STDERR_LIMIT - pending.bytes.len()];
            assert!(!pending.push(&fill));
            assert_eq!(pending.bytes.len(), PRE_NEGOTIATION_STDERR_LIMIT);
            assert_eq!(pending.bytes.capacity(), PRE_NEGOTIATION_STDERR_LIMIT);
            assert!(pending.push(b"overflow"));
            assert_eq!(pending.bytes, b"kept\n");
            assert!(pending.discard_partial);
            for _ in 0..64 {
                assert!(!pending.push(&fill));
                assert_eq!(pending.bytes, b"kept\n");
                assert_eq!(pending.bytes.capacity(), PRE_NEGOTIATION_STDERR_LIMIT);
            }
            assert!(!pending.push(b"\ndiscard this complete line too\n"));
            assert!(!pending.discard_partial);
            assert_eq!(pending.bytes, b"kept\n");
        }

        #[test]
        fn oversized_first_line_is_not_retained_as_a_runtime_prefix() {
            let mut pending = PendingStderr::default();
            assert!(!pending.push(crate::events::EVENT_MARKER.as_bytes()));
            assert!(pending.push(&vec![b' '; PRE_NEGOTIATION_STDERR_LIMIT]));
            assert!(pending.bytes.is_empty());
            assert!(pending.discard_partial);
            assert!(!pending.push(b"\n"));
            assert!(!pending.discard_partial);
            assert!(pending.bytes.is_empty());
        }

        #[tokio::test]
        async fn discarded_partial_line_preserves_exact_following_bytes() {
            let mut stderr = BufReader::with_capacity(3, &b"partial suffix\nnext exact line\n"[..]);
            assert!(discard_stderr_line(&mut stderr).await);
            let mut remaining = Vec::new();
            stderr.read_to_end(&mut remaining).await.unwrap();
            assert_eq!(remaining, b"next exact line\n");
            let mut stderr = BufReader::with_capacity(3, &b"partial at EOF"[..]);
            assert!(!discard_stderr_line(&mut stderr).await);
            assert!(stderr.fill_buf().await.unwrap().is_empty());
        }

        #[test]
        fn real_peer_exceeds_production_budget_before_ack_and_keeps_later_ownership() {
            use crate::events::{self, RuntimeEvent};
            const PROBE: &str = "KIT_TEST_STDERR_BOUND_PEER";
            if std::env::var_os(PROBE).is_some() {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let root = tempfile::tempdir().unwrap();
                        let script = root.path().join("stderr-bound.py");
                        std::fs::write(&script, r#"
import json, sys
mode, marker, budget = sys.argv[1], sys.argv[2], int(sys.argv[3])
def response(request, result):
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
def event(operation, call):
    return json.dumps({'event': 'child_started', 'call': call, 'tool': call,
                       'summary': call, 'at': 1, 'operation': operation})
def emit(operation, call):
    sys.stderr.write(marker + event(operation, call) + '\n')
    sys.stderr.flush()
for line in sys.stdin:
    request = json.loads(line)
    method, params = request['method'], request.get('params', {})
    if method == 'initialize':
        assert params['_meta']['kitDiagnosticOwnership'] == {'version': 1, 'transport': 'stderr'}
        if mode == 'complete':
            line = marker + event(None, 'discard-before-ack') + '\n'
            sys.stderr.write(line * (4 * budget // len(line) + 1))
        else:
            prefix = marker if mode == 'split-marker' else 'ordinary partial line:'
            sys.stderr.write(prefix + ' ' * (4 * budget))
        sys.stderr.flush()  # Cannot acknowledge until the pipe has drained > production cap.
        response(request, {'protocolVersion': 1, 'agentCapabilities': {'sessionCapabilities': {'close': {}}},
                           '_meta': {'kitDiagnosticOwnership': {'version': 1, 'transport': 'stderr'}}})
    elif method == 'session/new':
        operation = params['_meta']['kitDiagnosticOperation']
        assert operation
        # This request proves the parent accepted the initialize response. The
        # suspicious suffix still belongs to the oversized pre-ack partial line.
        if mode != 'complete':
            prefix = '' if mode == 'split-marker' else marker
            sys.stderr.write(prefix + event(operation, 'suspicious-suffix') + '\n')
            sys.stderr.flush()
        emit(operation, 'later-new')
        response(request, {'sessionId': 'bound-peer'})
    elif method == 'session/prompt':
        emit(params['_meta']['kitDiagnosticOperation'], 'later-prompt')
        response(request, {'stopReason': 'end_turn'})
    elif method == 'session/close':
        emit(params['_meta']['kitDiagnosticOperation'], 'later-close')
        response(request, {})
"#).unwrap();
                        for mode in ["complete", "split-marker", "runtime-suffix"] {
                            let harnesses = AcpHarnesses::new(BTreeMap::from([(
                                "stderr-bound".into(),
                                AcpHarnessProfile {
                                    command: "python3".into(),
                                    args: vec![
                                        script.to_string_lossy().into_owned(),
                                        mode.into(),
                                        events::EVENT_MARKER.into(),
                                        PRE_NEGOTIATION_STDERR_LIMIT.to_string(),
                                    ],
                                    permissions: AcpPermissionPolicy::Deny,
                                },
                            )])).unwrap();
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
                                default_harness: "acp.stderr-bound".into(),
                                parent_id: None,
                                parent_name: None,
                            };
                            tokio::time::timeout(Duration::from_secs(10), async {
                                let base = owned_child_invocation(mode, "launch", ChildSession::start(
                                    config, "acp.stderr-bound".into(), None, None, 1, TurnCancellation::default(),
                                )).await.unwrap();
                                assert!(base.diagnostics.negotiated.load(Ordering::Acquire));
                                owned_child_invocation(mode, "new", base.prompt("later".into(), TurnCancellation::default())).await.unwrap();
                                owned_child_invocation(mode, "close", base.close()).await.unwrap();
                                let mut closed = base.closed_signal();
                                let registry = Arc::downgrade(&base.diagnostics);
                                drop(base);
                                closed.wait_for(|closed| *closed).await.unwrap();
                                while registry.upgrade().is_some() {
                                    tokio::task::yield_now().await;
                                }
                            }).await.expect("no stderr backpressure deadlock or negotiation failure");
                        }
                    });
                return;
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "acp_child::tests::pre_negotiation_stderr::real_peer_exceeds_production_budget_before_ack_and_keeps_later_ownership",
                    "--nocapture",
                ])
                .env(PROBE, "1")
                .env(events::EVENTS_ENV, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stderr = String::from_utf8(output.stderr).unwrap();
            let warning = format!(
                "pre-negotiation stderr exceeded {PRE_NEGOTIATION_STDERR_LIMIT}-byte limit"
            );
            assert_eq!(stderr.matches(&warning).count(), 3, "{stderr}");
            let diagnostics = stderr
                .lines()
                .filter_map(events::parse_diagnostic)
                .collect::<Vec<_>>();
            for mode in ["complete", "split-marker", "runtime-suffix"] {
                for (call, owner) in [
                    ("later-new", "launch"),
                    ("later-prompt", "new"),
                    ("later-close", "close"),
                ] {
                    let operation = DiagnosticOperation::new(format!("{mode}:{owner}")).unwrap();
                    let matches = diagnostics.iter().filter(|envelope| {
                        envelope.operation.as_ref() == Some(&operation)
                            && matches!(&envelope.event, RuntimeEvent::ChildStarted { call: actual, .. } if actual == call)
                    }).collect::<Vec<_>>();
                    assert_eq!(
                        matches.len(),
                        1,
                        "missing or duplicate owned {mode}/{call}: {stderr}"
                    );
                    let admission = diagnostics
                        .iter()
                        .find(|envelope| {
                            envelope.operation.as_ref() == Some(&operation)
                                && matches!(envelope.event, RuntimeEvent::SessionStarted { .. })
                        })
                        .unwrap();
                    assert!(matches[0].activation.is_some());
                    assert_eq!(matches[0].activation, admission.activation);
                }
            }
            assert!(
                !diagnostics.iter().any(|envelope| matches!(
                    &envelope.event, RuntimeEvent::ChildStarted { call, .. }
                        if call == "suspicious-suffix" || call == "discard-before-ack"
                )),
                "discarded bytes must never become owned runtime events: {stderr}"
            );
            assert!(
                !stderr.contains("suspicious-suffix"),
                "partial suffix must be discarded, not printed"
            );
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

            forward_stderr(stderr, "acp.kit", Some("s-owner"), None, |item| {
                output.push(item)
            })
            .await;
            let status = child.wait().await.unwrap();

            assert_eq!(status.code(), Some(23));
            let forwarded = output
                .iter()
                .filter_map(|item| match item {
                    ForwardedStderr::RuntimeLine(line) => Some(line.clone()),
                    ForwardedStderr::Diagnostic(_)
                    | ForwardedStderr::Cleanup(_)
                    | ForwardedStderr::OwnedCleanup(_, _) => None,
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
                ForwardedStderr::Diagnostic(_)
                | ForwardedStderr::Cleanup(_)
                | ForwardedStderr::OwnedCleanup(_, _) => false,
            }));
        }

        #[tokio::test]
        async fn emits_cleanup_once_on_normal_eof() {
            let mut output = Vec::new();
            forward_stderr(&b""[..], "acp.kit", Some("s-owner"), None, |item| {
                output.push(item)
            })
            .await;
            assert_eq!(cleanup_count(&output), 1);
        }

        #[tokio::test]
        async fn emits_cleanup_once_on_abrupt_stream_error() {
            let mut output = Vec::new();
            forward_stderr(&b"\xff"[..], "acp.kit", Some("s-owner"), None, |item| {
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
