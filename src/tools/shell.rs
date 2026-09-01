use std::{path::PathBuf, process::Stdio, time::Duration};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::{io::AsyncReadExt, process::Command};

use crate::process_tree::{isolate_tokio_process_tree, terminate_tokio_process_tree};

const MAX_INTERNAL_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct ShellTool {
    root: PathBuf,
    spec: ToolSpec,
}

impl ShellTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            spec: ToolSpec::new(
                ToolName::new("shell"),
                "Run a shell command from Kit's working directory. Commands can access any path allowed to the Kit process.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 3600, "default": 120}
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            )
            .with_output_schema(json!({
                "type": "object",
                "properties": {
                    "exit_code": {"type": ["integer", "null"]},
                    "success": {"type": "boolean"},
                    "stdout": {"type": "string"},
                    "stderr": {"type": "string"}
                },
                "required": ["exit_code", "success", "stdout", "stderr"],
                "additionalProperties": false
            }))
            .with_annotations(ToolAnnotations::new()),
        }
    }
}

#[derive(Deserialize)]
struct ShellInput {
    command: String,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let cancellation = context.cancellation.clone();
        let input: ShellInput = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if input.command.is_empty() || !(1..=3600).contains(&input.timeout_seconds) {
            return Err(ToolError::InvalidInput(
                "command and timeout_seconds are outside bounds".into(),
            ));
        }
        let mut command = shell_command(&input.command);
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        isolate_tokio_process_tree(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        let pid = child
            .id()
            .ok_or_else(|| ToolError::Internal("spawned shell did not have a process ID".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Internal("shell stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Internal("shell stderr was not piped".into()))?;
        let mut stdout_task = tokio::spawn(read_output(stdout));
        let mut stderr_task = tokio::spawn(read_output(stderr));
        let mut stdout_finished = false;
        let mut stderr_finished = false;
        let mut status = None;
        let mut stdout = None;
        let mut stderr = None;
        let timeout = tokio::time::sleep(Duration::from_secs(input.timeout_seconds));
        tokio::pin!(timeout);
        // A cancelled turn must not wait out the command: the loop awaits this
        // invocation, so an uncooperative tool keeps the whole turn alive until
        // the timeout, however long the caller asked for.
        let interrupted = async {
            match &cancellation {
                Some(cancellation) => cancellation.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(interrupted);

        while status.is_none() || stdout.is_none() || stderr.is_none() {
            let event = tokio::select! {
                result = child.wait(), if status.is_none() => ShellEvent::Wait(result),
                result = &mut stdout_task, if !stdout_finished => ShellEvent::Stdout(result),
                result = &mut stderr_task, if !stderr_finished => ShellEvent::Stderr(result),
                () = &mut timeout => ShellEvent::Timeout,
                () = &mut interrupted => ShellEvent::Cancelled,
            };
            match event {
                ShellEvent::Wait(Ok(exit_status)) => status = Some(exit_status),
                ShellEvent::Wait(Err(error)) => {
                    terminate_shell(
                        &mut child,
                        pid,
                        &mut stdout_task,
                        stdout_finished,
                        &mut stderr_task,
                        stderr_finished,
                    )
                    .await;
                    return Err(ToolError::ExecutionFailed(error.to_string()));
                }
                ShellEvent::Stdout(result) => {
                    stdout_finished = true;
                    match output_task_result(result) {
                        Ok(output) => stdout = Some(output),
                        Err(error) => {
                            terminate_shell(
                                &mut child,
                                pid,
                                &mut stdout_task,
                                stdout_finished,
                                &mut stderr_task,
                                stderr_finished,
                            )
                            .await;
                            return Err(error);
                        }
                    }
                }
                ShellEvent::Stderr(result) => {
                    stderr_finished = true;
                    match output_task_result(result) {
                        Ok(output) => stderr = Some(output),
                        Err(error) => {
                            terminate_shell(
                                &mut child,
                                pid,
                                &mut stdout_task,
                                stdout_finished,
                                &mut stderr_task,
                                stderr_finished,
                            )
                            .await;
                            return Err(error);
                        }
                    }
                }
                ShellEvent::Timeout => {
                    terminate_shell(
                        &mut child,
                        pid,
                        &mut stdout_task,
                        stdout_finished,
                        &mut stderr_task,
                        stderr_finished,
                    )
                    .await;
                    return Err(ToolError::ExecutionFailed("shell command timed out".into()));
                }
                ShellEvent::Cancelled => {
                    terminate_shell(
                        &mut child,
                        pid,
                        &mut stdout_task,
                        stdout_finished,
                        &mut stderr_task,
                        stderr_finished,
                    )
                    .await;
                    return Err(ToolError::Cancelled);
                }
            }
        }
        let status =
            status.ok_or_else(|| ToolError::Internal("shell status was not collected".into()))?;
        let stdout =
            stdout.ok_or_else(|| ToolError::Internal("shell stdout was not collected".into()))?;
        let stderr =
            stderr.ok_or_else(|| ToolError::Internal("shell stderr was not collected".into()))?;
        let output = json!({
            "exit_code": status.code(),
            "success": status.success(),
            "stdout": stdout,
            "stderr": stderr
        });
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(output),
        )))
    }
}

type OutputTask = tokio::task::JoinHandle<std::io::Result<String>>;

enum ShellEvent {
    Wait(std::io::Result<std::process::ExitStatus>),
    Stdout(Result<std::io::Result<String>, tokio::task::JoinError>),
    Stderr(Result<std::io::Result<String>, tokio::task::JoinError>),
    Timeout,
    Cancelled,
}

fn output_task_result(
    result: Result<std::io::Result<String>, tokio::task::JoinError>,
) -> Result<String, ToolError> {
    result
        .map_err(|error| ToolError::Internal(error.to_string()))?
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))
}

async fn terminate_shell(
    child: &mut tokio::process::Child,
    pid: u32,
    stdout_task: &mut OutputTask,
    stdout_finished: bool,
    stderr_task: &mut OutputTask,
    stderr_finished: bool,
) {
    terminate_tokio_process_tree(child, pid).await;
    abort_output_task(stdout_task, stdout_finished).await;
    abort_output_task(stderr_task, stderr_finished).await;
}

async fn abort_output_task(task: &mut OutputTask, finished: bool) {
    if !finished {
        task.abort();
        let _ = task.await;
    }
}

async fn read_output(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<String> {
    let mut content = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if content.len() + read > MAX_INTERNAL_OUTPUT_BYTES {
            return Err(std::io::Error::other(format!(
                "shell output exceeds {MAX_INTERNAL_OUTPUT_BYTES} bytes"
            )));
        }
        content.extend_from_slice(&buffer[..read]);
    }
    Ok(String::from_utf8(content)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
}

#[cfg(unix)]
fn shell_command(input: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-lc").arg(input);
    command
}

#[cfg(windows)]
fn shell_command(input: &str) -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg(input);
    command
}

const fn default_timeout() -> u64 {
    120
}

#[cfg(all(test, unix))]
mod tests {
    use std::{sync::Arc, time::Duration};

    use agentkit_core::{MetadataMap, SessionId, TurnId};
    use agentkit_tools_core::{
        AllowAllPermissions, OwnedToolContext, Tool as _, ToolError, ToolRequest,
    };
    use serde_json::json;
    use tokio::io::AsyncWriteExt as _;

    use super::{MAX_INTERNAL_OUTPUT_BYTES, ShellTool, read_output, shell_command};
    use crate::process_tree::{isolate_tokio_process_tree, terminate_tokio_process_tree};

    async fn invoke_shell(
        root: &std::path::Path,
        command: &str,
        timeout_seconds: u64,
    ) -> Result<(), ToolError> {
        let tool = ShellTool::new(root.to_path_buf());
        let context = OwnedToolContext {
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };
        let request = ToolRequest::new(
            "call",
            "shell",
            json!({
                "command": command,
                "timeout_seconds": timeout_seconds,
            }),
            "session",
            "turn",
        );
        tool.invoke(request, &mut context.borrowed())
            .await
            .map(|_| ())
    }

    fn assert_output_limit(error: ToolError) {
        let ToolError::ExecutionFailed(message) = error else {
            panic!("unexpected shell error: {error}");
        };
        assert_eq!(
            message,
            format!("shell output exceeds {MAX_INTERNAL_OUTPUT_BYTES} bytes")
        );
    }

    async fn assert_process_exited(pid: i32) {
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("process {pid} survived shell termination");
    }

    #[tokio::test]
    async fn oversized_output_stays_complete() {
        let data = [vec![b'A'; 10 * 1024], vec![b'Z'; 10 * 1024]].concat();
        let (mut writer, reader) = tokio::io::duplex(data.len() + 1);
        writer.write_all(&data).await.unwrap();
        writer.shutdown().await.unwrap();

        let captured = read_output(reader).await.unwrap();

        assert_eq!(captured.as_bytes(), data);
    }

    #[tokio::test]
    async fn small_output_stays_complete() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"small output").await.unwrap();
        writer.shutdown().await.unwrap();

        let captured = read_output(reader).await.unwrap();

        assert_eq!(captured, "small output");
    }

    #[tokio::test]
    async fn stdout_output_limit_terminates_shell() {
        let directory = tempfile::tempdir().unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            invoke_shell(directory.path(), "yes stdout", 5),
        )
        .await
        .expect("stdout output limit did not stop the shell")
        .unwrap_err();

        assert_output_limit(error);
    }

    #[tokio::test]
    async fn stderr_output_limit_terminates_shell() {
        let directory = tempfile::tempdir().unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            invoke_shell(directory.path(), "yes stderr >&2", 5),
        )
        .await
        .expect("stderr output limit did not stop the shell")
        .unwrap_err();

        assert_output_limit(error);
    }

    #[tokio::test]
    async fn parent_exit_with_descendant_pipe_times_out_and_kills_descendant() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("descendant.pid");
        let command = format!("sleep 30 & echo $! > {}", pid_file.display());
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            invoke_shell(directory.path(), &command, 1),
        )
        .await
        .expect("descendant pipe kept the shell invocation alive")
        .unwrap_err();
        let ToolError::ExecutionFailed(message) = error else {
            panic!("unexpected shell error: {error}");
        };
        assert_eq!(message, "shell command timed out");
        let descendant = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        assert_process_exited(descendant).await;
    }

    #[tokio::test]
    async fn cancellation_kills_shell_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("child.pid");
        let mut command = shell_command(&format!(
            "sleep 30 & echo $! > {}; wait",
            pid_file.display()
        ));
        isolate_tokio_process_tree(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let descendant = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = contents.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shell did not report its descendant PID");

        terminate_tokio_process_tree(&mut child, pid).await;

        assert_process_exited(descendant).await;
    }
}
