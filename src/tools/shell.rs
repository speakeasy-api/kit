use std::{path::PathBuf, process::Stdio, time::Duration};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::{io::AsyncReadExt, process::Command};

const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

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
                "Run a shell command with the runtime root as its working directory.",
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
        isolate_process_tree(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Internal("shell stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Internal("shell stderr was not piped".into()))?;
        let mut stdout_task = tokio::spawn(read_bounded(stdout));
        let mut stderr_task = tokio::spawn(read_bounded(stderr));
        let execution = async {
            let status = child
                .wait()
                .await
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            let stdout = (&mut stdout_task)
                .await
                .map_err(|error| ToolError::Internal(error.to_string()))?
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            let stderr = (&mut stderr_task)
                .await
                .map_err(|error| ToolError::Internal(error.to_string()))?
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            Ok::<_, ToolError>((status, stdout, stderr))
        };
        // A cancelled turn must not wait out the command: the loop awaits this
        // invocation, so an uncooperative tool keeps the whole turn alive until
        // the timeout, however long the caller asked for.
        let interrupted = async {
            match &cancellation {
                Some(cancellation) => cancellation.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let finished = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(input.timeout_seconds), execution) => Some(result),
            () = interrupted => None,
        };
        // The command's futures are dropped with the select, so the child is
        // ours to kill again.
        let (status, stdout, stderr) = match finished {
            Some(Ok(result)) => result?,
            outcome => {
                kill_process_tree(&mut child).await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(match outcome {
                    Some(_) => ToolError::ExecutionFailed("shell command timed out".into()),
                    None => ToolError::Cancelled,
                });
            }
        };
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(json!({
                "exit_code": status.code(),
                "success": status.success(),
                "stdout": stdout,
                "stderr": stderr
            })),
        )))
    }
}

#[cfg(unix)]
fn isolate_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // A shell is only the group leader; cancelling it must also stop pipelines,
    // sleeps, and other descendants it launched.
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn isolate_process_tree(_command: &mut Command) {}

async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id()
        && let Ok(pid) = i32::try_from(pid)
    {
        // The child was created as its own process-group leader.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .await;
    }
    let _ = child.kill().await;
}

async fn read_bounded(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<String> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if truncated {
        output.push_str("\n[output truncated]");
    }
    Ok(output)
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
    use std::time::Duration;

    use super::{isolate_process_tree, kill_process_tree, shell_command};

    #[tokio::test]
    async fn cancellation_kills_shell_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("child.pid");
        let mut command = shell_command(&format!(
            "sleep 30 & echo $! > {}; wait",
            pid_file.display()
        ));
        isolate_process_tree(&mut command);
        let mut child = command.spawn().unwrap();
        while !pid_file.exists() {
            tokio::task::yield_now().await;
        }
        let descendant: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        kill_process_tree(&mut child).await;

        for _ in 0..100 {
            if unsafe { libc::kill(descendant, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("shell descendant {descendant} survived cancellation");
    }
}
