//! Generic parent-owned ACP subprocesses used for nested agents.

use std::{
    collections::{BTreeMap, HashMap},
    io::Cursor,
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
    RequestPermissionResponse, SelectedPermissionOutcome, SessionId, SessionNotification,
    SessionUpdate, StopReason,
};
use agentkit_core::TurnCancellation;
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::tools::mcp::CredentialStorage;

const HANDSHAKE: Duration = Duration::from_secs(30);
const CANCEL_SETTLE: Duration = Duration::from_secs(5);
const MAX_CAPTURED_UPDATES: usize = 64;
const MAX_CAPTURED_UPDATE_BYTES: usize = 64 * 1024;
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
#[serde(deny_unknown_fields)]
pub struct AcpHarnessProfile {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub permissions: AcpPermissionPolicy,
}

/// Validated named ACP harness profiles. `acp.kit` is always Kit; its launch base may be overridden.
#[derive(Clone, Debug, Default)]
pub struct AcpHarnesses(Arc<BTreeMap<String, AcpHarnessProfile>>);

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
        Ok(Self(Arc::new(profiles)))
    }

    pub fn contains(&self, reference: &str) -> bool {
        self.profile_name(reference)
            .is_some_and(|name| name == "kit" || self.0.contains_key(name))
    }

    pub fn is_kit(&self, reference: &str) -> bool {
        reference == BUILTIN_HARNESS
    }

    pub fn references(&self) -> Vec<String> {
        std::iter::once(BUILTIN_HARNESS.to_string())
            .chain(
                self.0
                    .keys()
                    .filter(|name| name.as_str() != "kit")
                    .map(|name| format!("acp.{name}")),
            )
            .collect()
    }

    fn profile_name<'a>(&self, reference: &'a str) -> Option<&'a str> {
        let name = reference.strip_prefix("acp.")?;
        (!name.is_empty() && !name.contains('.')).then_some(name)
    }

    fn launch_context(&self, reference: &str) -> LaunchContext {
        let source = if reference == BUILTIN_HARNESS && !self.0.contains_key("kit") {
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
                .0
                .get(name)
                .map_or(AcpPermissionPolicy::Deny, |profile| profile.permissions))
        } else {
            self.0
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
                .0
                .get(name)
                .ok_or_else(|| format!("unknown ACP harness {reference:?}"))?;
            let mut command = Command::new(&profile.command);
            command.args(&profile.args);
            command
        };
        // Every trusted profile is spawned directly (never through a shell)
        // with the runtime root as cwd.
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
        let mut command = if let Some(profile) = self.0.get("kit") {
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
            .arg("--session-id")
            .arg(id)
            .arg("--subagent-depth")
            .arg(depth.to_string());
        if resume {
            command.arg("--resume");
        }
        if let Some(path) = &config.mcp_config {
            command.arg("--mcp-config").arg(path);
        }
        command
            .arg("--mcp-credential-store")
            .arg(config.credential_storage.cli_name());
        if let Some(path) = config.credential_storage.directory() {
            command.arg("--mcp-credential-dir").arg(path);
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
            "ACP harness {phase}: {error} (harness={:?}, source={}, cwd=runtime root)",
            self.harness, self.source
        )
    }
}

/// The combined `kit serve` command used by the TUI.
pub(crate) fn serve_command(
    root: &Path,
    model: &str,
    provider: crate::ProviderKind,
    session_id: &str,
    resume: bool,
) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--model")
        .arg(model)
        .arg("--provider")
        .arg(provider.as_str())
        .arg("--session-id")
        .arg(session_id);
    if resume {
        command.arg("--resume");
    }
    Ok(command)
}

#[derive(Clone)]
pub(crate) struct ChildConfig {
    pub root: PathBuf,
    pub model: String,
    pub provider: crate::ProviderKind,
    pub mcp_config: Option<PathBuf>,
    pub credential_storage: CredentialStorage,
    pub harnesses: AcpHarnesses,
    pub default_harness: String,
}

#[derive(Debug)]
pub(crate) enum ChildError {
    Cancelled,
    Failed(String),
}
impl std::fmt::Display for ChildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("nested agent cancelled"),
            Self::Failed(e) => f.write_str(e),
        }
    }
}

struct Prompt {
    session_id: SessionId,
    text: String,
    cancellation: TurnCancellation,
    reply: oneshot::Sender<Result<ChildOutput, ChildError>>,
}
struct Fork {
    session_id: SessionId,
    cancellation: TurnCancellation,
    reply: oneshot::Sender<Result<SessionId, ChildError>>,
}
enum Request {
    Prompt(Prompt),
    Fork(Fork),
}
struct Ready {
    session_id: SessionId,
    capabilities: agentkit_acp::AgentCapabilities,
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
        let remaining = MAX_CAPTURED_UPDATE_BYTES - self.update_bytes;
        let mut encoded = vec![0; remaining];
        let mut writer = Cursor::new(encoded.as_mut_slice());
        if serde_json::to_writer(&mut writer, &update).is_err() {
            self.updates_truncated = true;
            return;
        }
        let length = writer.position() as usize;
        encoded.truncate(length);
        match serde_json::from_slice(&encoded) {
            Ok(value) => {
                self.updates.push(value);
                self.update_bytes += length;
            }
            Err(_) => self.updates_truncated = true,
        }
    }
}

/// A logical ACP session. Multiple forked sessions may share one child process.
#[derive(Clone)]
pub(crate) struct ChildSession {
    tx: mpsc::Sender<Request>,
    session_id: SessionId,
    capabilities: agentkit_acp::AgentCapabilities,
    serial: Arc<tokio::sync::Mutex<()>>,
}

impl ChildSession {
    pub async fn start(
        config: ChildConfig,
        harness: String,
        persisted: Option<(String, bool)>,
        depth: usize,
        cancellation: TurnCancellation,
    ) -> Result<Self, ChildError> {
        let context = config.harnesses.launch_context(&harness);
        let actor_context = context.clone();
        let (tx, mut rx) = mpsc::channel(1);
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let actor_tx = tx.clone();
        let mut task = tokio::spawn(async move {
            run(
                config,
                harness,
                persisted,
                depth,
                &mut rx,
                ready_tx,
                actor_context,
            )
            .await
        });
        let result = tokio::select! {
            ready = &mut ready_rx => match ready {
                Ok(ready) => Ok(ready),
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
                tx: actor_tx,
                session_id: ready.session_id,
                capabilities: ready.capabilities,
                serial: Arc::new(tokio::sync::Mutex::new(())),
            }),
            Err(error) => {
                task.abort();
                let _ = task.await;
                Err(error)
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
    pub fn supports_native_fork(&self) -> bool {
        self.capabilities.session_capabilities.fork.is_some()
    }

    #[cfg(test)]
    pub(crate) fn disconnected_for_test() -> Self {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        Self {
            tx,
            session_id: "test".into(),
            capabilities: agentkit_acp::AgentCapabilities::default(),
            serial: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn fork(&self, cancellation: &TurnCancellation) -> Result<Self, ChildError> {
        let _serial = tokio::select! {
            serial = self.serial.lock() => serial,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        if !self.supports_native_fork() {
            return Err(ChildError::Failed(
                "ACP harness does not support session/fork".into(),
            ));
        }
        let (reply, response) = oneshot::channel();
        tokio::select! {
            sent = self.tx.send(Request::Fork(Fork {
                session_id: self.session_id.clone(),
                cancellation: cancellation.clone(),
                reply,
            })) => sent.map_err(|_| ChildError::Failed("nested agent process is no longer running".into()))?,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        }
        let session_id = response.await.map_err(|_| {
            ChildError::Failed("nested agent process exited without a fork response".into())
        })??;
        Ok(Self {
            tx: self.tx.clone(),
            session_id,
            capabilities: self.capabilities.clone(),
            serial: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub async fn prompt(
        &self,
        text: String,
        cancellation: TurnCancellation,
    ) -> Result<ChildOutput, ChildError> {
        let _serial = tokio::select! {
            serial = self.serial.lock() => serial,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        let (reply, response) = oneshot::channel();
        let request = Request::Prompt(Prompt {
            session_id: self.session_id.clone(),
            text,
            cancellation: cancellation.clone(),
            reply,
        });
        tokio::select! {
            sent = self.tx.send(request) => sent.map_err(|_| ChildError::Failed("nested agent process is no longer running".into()))?,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        }
        response.await.map_err(|_| {
            ChildError::Failed("nested agent process exited without a response".into())
        })?
    }
}

async fn run(
    config: ChildConfig,
    harness: String,
    persisted: Option<(String, bool)>,
    depth: usize,
    rx: &mut mpsc::Receiver<Request>,
    ready: oneshot::Sender<Ready>,
    context: LaunchContext,
) -> Result<(), String> {
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
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if matches!(
                crate::events::parse(&line),
                Some(
                    crate::events::RuntimeEvent::ChildStarted { .. }
                        | crate::events::RuntimeEvent::ChildFinished { .. }
                )
            ) {
                // Only nested tool events belong to this process's runtime graph.
                // A harness's compaction changes its own transcript, not ours.
                eprintln!("{line}");
            } else {
                eprintln!("ACP harness {label}: {line}");
            }
        }
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
            let sessions = Arc::new(Mutex::new(vec![session.session_id.clone()]));
            let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel();
            let mut tasks = JoinSet::new();
            ready_flag.store(true, Ordering::Release);
            let _ = ready.send(Ready { session_id: session.session_id, capabilities });
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
                        let fatal = fatal_tx.clone();
                        tasks.spawn(async move {
                            let request = connection
                                .send_request(ForkSessionRequest::new(fork.session_id, root))
                                .block_task();
                            tokio::pin!(request);
                            let result = tokio::select! {
                                result = &mut request => result
                                    .map(|response| {
                                        if let Ok(mut sessions) = sessions.lock() {
                                            sessions.push(response.session_id.clone());
                                        }
                                        response.session_id
                                    })
                                    .map_err(|error| ChildError::Failed(error.to_string())),
                                () = fork.cancellation.cancelled() => {
                                    let _ = fatal.send(());
                                    Err(ChildError::Cancelled)
                                }
                                () = tokio::time::sleep(HANDSHAKE) => {
                                    let _ = fatal.send(());
                                    Err(ChildError::Failed(format!(
                                        "ACP harness did not answer session/fork within {} seconds",
                                        HANDSHAKE.as_secs()
                                    )))
                                }
                            };
                            let _ = fork.reply.send(result);
                        });
                    }
                    Request::Prompt(prompt) => {
                        let connection = connection.clone();
                        let routes = Arc::clone(&routes);
                        let fatal = fatal_tx.clone();
                        tasks.spawn(async move {
                            let session_id = prompt.session_id.clone();
                            let output = Arc::new(Mutex::new(ChildOutput::default()));
                            if let Ok(mut routes) = routes.lock() { routes.insert(session_id.clone(), Arc::clone(&output)); }
                            let request = connection.send_request(agentkit_acp::PromptRequest::new(
                                session_id.clone(), vec![ContentBlock::Text(agentkit_acp::TextContent::new(prompt.text))],
                            )).block_task();
                            tokio::pin!(request);
                            let (response, cancelled) = tokio::select! {
                                biased;
                                result = &mut request => (result.map_err(|error| error.to_string()), false),
                                () = prompt.cancellation.cancelled() => {
                                    let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
                                    match tokio::time::timeout(CANCEL_SETTLE, &mut request).await {
                                        Ok(result) => (result.map_err(|error| error.to_string()), true),
                                        Err(_) => {
                                            let _ = prompt.reply.send(Err(ChildError::Cancelled));
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
        })
        .await;
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
mod tests {
    use serde_json::json;

    use super::*;

    fn update(value: Value) -> SessionUpdate {
        serde_json::from_value(value).unwrap()
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
        assert!(error.contains("cwd=runtime root"));
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
            mcp_config: None,
            credential_storage: Default::default(),
            harnesses: AcpHarnesses::new(profiles).unwrap(),
            default_harness: "acp.broken".into(),
        };
        let result = ChildSession::start(
            config,
            "acp.broken".into(),
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
        assert!(error.contains("cwd=runtime root"), "{error}");
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
            mcp_config: None,
            credential_storage: Default::default(),
            harnesses: harnesses.clone(),
            default_harness: "acp.other".into(),
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
        let config = ChildConfig {
            root: root.path().to_path_buf(),
            model: "test-model".into(),
            provider: crate::ProviderKind::OpenRouter,
            mcp_config: None,
            credential_storage: Default::default(),
            harnesses: harnesses.clone(),
            default_harness: BUILTIN_HARNESS.into(),
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
                .any(|pair| pair == ["--session-id", "session"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--subagent-depth", "2"])
        );
        assert!(args.iter().any(|arg| arg == "--root"));
        assert!(args.iter().any(|arg| arg == "--resume"));
        assert!(args.iter().any(|arg| arg == "--mcp-credential-store"));
        assert_eq!(command.as_std().get_current_dir(), Some(root.path()));
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
            mcp_config: None,
            credential_storage: Default::default(),
            harnesses: AcpHarnesses::new(profiles).unwrap(),
            default_harness: "acp.broken".into(),
        };
        let starts = (0..64)
            .map(|_| {
                let config = config.clone();
                tokio::spawn(async move {
                    ChildSession::start(
                        config,
                        "acp.broken".into(),
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
            assert!(error.contains("cwd=runtime root"), "{error}");
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
            mcp_config: None,
            credential_storage: Default::default(),
            harnesses,
            default_harness: "acp.mock".into(),
        };
        let base = ChildSession::start(
            config,
            "acp.mock".into(),
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
        let first = base.fork(&TurnCancellation::default()).await.unwrap();
        let second = base.fork(&TurnCancellation::default()).await.unwrap();
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

        let same_session = base.fork(&TurnCancellation::default()).await.unwrap();
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
