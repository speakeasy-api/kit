//! Parent-owned Kit subprocesses used for nested agents.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use agent_client_protocol::{ByteStreams, schema::ProtocolVersion};
use agentkit_acp::{
    CancelNotification, CloseSessionRequest, ContentBlock, PromptResponse, SessionNotification,
    SessionUpdate, StopReason,
};
use agentkit_core::TurnCancellation;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::tools::mcp::CredentialStorage;

const HANDSHAKE: Duration = Duration::from_secs(30);
const CANCEL_SETTLE: Duration = Duration::from_secs(5);

/// The common `kit serve` command line used by the TUI and nested agents.
pub(crate) fn serve_command(
    root: &Path,
    model: &str,
    session_id: &str,
    resume: bool,
    depth: usize,
    disable_a2a: bool,
) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--model")
        .arg(model)
        .arg("--session-id")
        .arg(session_id)
        .arg("--subagent-depth")
        .arg(depth.to_string());
    if resume {
        command.arg("--resume");
    }
    if disable_a2a {
        command.arg("--no-a2a");
    }
    Ok(command)
}

#[derive(Clone)]
pub(crate) struct ChildConfig {
    pub root: PathBuf,
    pub model: String,
    pub mcp_config: Option<PathBuf>,
    pub credential_storage: CredentialStorage,
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
    text: String,
    cancellation: TurnCancellation,
    reply: oneshot::Sender<Result<String, ChildError>>,
}

/// A cloneable command port; the owning actor retains and cleans up the process.
#[derive(Clone)]
pub(crate) struct ChildSession {
    tx: mpsc::Sender<Prompt>,
}

impl ChildSession {
    pub async fn start(
        config: ChildConfig,
        id: String,
        resume: bool,
        depth: usize,
        cancellation: TurnCancellation,
    ) -> Result<Self, ChildError> {
        let (tx, mut rx) = mpsc::channel::<Prompt>(1);
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let mut task =
            tokio::spawn(async move { run(config, id, resume, depth, &mut rx, ready_tx).await });
        let timeout = tokio::time::sleep(HANDSHAKE);
        tokio::pin!(timeout);
        let result = tokio::select! {
            ready = &mut ready_rx => ready
                .map_err(|_| ChildError::Failed("nested agent exited during startup".into())),
            () = cancellation.cancelled() => Err(ChildError::Cancelled),
            () = &mut timeout => Err(ChildError::Failed(format!(
                "nested agent did not answer the ACP handshake within {} seconds",
                HANDSHAKE.as_secs()
            ))),
            joined = &mut task => Err(ChildError::Failed(match joined {
                Ok(Ok(())) => "nested agent exited during startup".into(),
                Ok(Err(error)) => error,
                Err(error) => format!("nested agent startup actor failed: {error}"),
            })),
        };
        match result {
            Ok(()) => Ok(Self { tx }),
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

    #[cfg(test)]
    pub(crate) fn disconnected_for_test() -> Self {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        Self { tx }
    }

    pub async fn prompt(
        &self,
        text: String,
        cancellation: TurnCancellation,
    ) -> Result<String, ChildError> {
        let (reply, response) = oneshot::channel();
        tokio::select! {
            sent = self.tx.send(Prompt { text, cancellation: cancellation.clone(), reply }) => {
                sent.map_err(|_| ChildError::Failed("nested agent process is no longer running".into()))?;
            }
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        }
        response.await.map_err(|_| {
            ChildError::Failed("nested agent process exited without a response".into())
        })?
    }
}

async fn run(
    config: ChildConfig,
    id: String,
    resume: bool,
    depth: usize,
    rx: &mut mpsc::Receiver<Prompt>,
    ready: oneshot::Sender<()>,
) -> Result<(), String> {
    let mut command = serve_command(&config.root, &config.model, &id, resume, depth, true)
        .map_err(|e| e.to_string())?;
    if let Some(path) = &config.mcp_config {
        command.arg("--mcp-config").arg(path);
    }
    command
        .arg("--mcp-credential-store")
        .arg(config.credential_storage.cli_name());
    if let Some(path) = config.credential_storage.directory() {
        command.arg("--mcp-credential-dir").arg(path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start nested Kit: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or("could not open nested Kit stdin")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("could not open nested Kit stdout")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("could not open nested Kit stderr")?;
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if crate::events::parse(&line).is_some() {
                eprintln!("{line}");
            } else {
                eprintln!("nested Kit: {line}");
            }
        }
    });
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let (chunks_tx, mut chunks_rx) = mpsc::unbounded_channel();
    let notifications = chunks_tx.clone();
    let root = config.root.clone();
    let connected = agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                    && let ContentBlock::Text(text) = chunk.content
                {
                    let _ = notifications.send(text.text);
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
                .send_request(agentkit_acp::NewSessionRequest::new(root))
                .block_task()
                .await?;
            let _ = ready.send(());
            while let Some(prompt) = rx.recv().await {
                while chunks_rx.try_recv().is_ok() {}
                let session_id = session.session_id.clone();
                let request = connection
                    .send_request(agentkit_acp::PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(agentkit_acp::TextContent::new(
                            prompt.text,
                        ))],
                    ))
                    .block_task();
                tokio::pin!(request);
                let (response, cancelled) = tokio::select! {
                    biased;
                    // Prefer an already completed response over a simultaneous interrupt.
                    result = &mut request => (result.map_err(|error| error.to_string()), false),
                    () = prompt.cancellation.cancelled() => {
                        let _ = connection.send_notification(CancelNotification::new(session_id));
                        match tokio::time::timeout(CANCEL_SETTLE, &mut request).await {
                            Ok(result) => (result.map_err(|error| error.to_string()), true),
                            Err(_) => {
                                let _ = prompt.reply.send(Err(ChildError::Cancelled));
                                return Err(agent_client_protocol::Error::internal_error().data(
                                    serde_json::json!("nested agent did not settle after cancellation"),
                                ));
                            }
                        }
                    }
                };
                let output = std::iter::from_fn(|| chunks_rx.try_recv().ok()).collect::<String>();
                let outcome = if cancelled {
                    Err(ChildError::Cancelled)
                } else {
                    response
                        .map_err(ChildError::Failed)
                        .and_then(|response| prompt_outcome(response, output))
                };
                let retire = outcome.is_err();
                let _ = prompt.reply.send(outcome);
                if retire {
                    return Err(agent_client_protocol::Error::internal_error().data(
                        serde_json::json!("nested agent prompt did not complete successfully"),
                    ));
                }
            }
            let close = connection
                .send_request(CloseSessionRequest::new(session.session_id))
                .block_task();
            match tokio::time::timeout(CANCEL_SETTLE, close).await {
                Ok(result) => {
                    result?;
                }
                Err(_) => {
                    return Err(agent_client_protocol::Error::internal_error().data(
                        serde_json::json!("nested agent did not close within the settle timeout"),
                    ));
                }
            }
            Ok(())
        })
        .await;
    let _ = child.kill().await;
    connected.map_err(|e| e.to_string())
}

fn prompt_outcome(response: PromptResponse, output: String) -> Result<String, ChildError> {
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
