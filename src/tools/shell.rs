use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const MAX_MODEL_OUTPUT_BYTES: usize = 8 * 1024;

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
                    "stderr": {"type": "string"},
                    "stdout_artifact": {"type": "string"},
                    "stderr_artifact": {"type": "string"}
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
        let artifact_directory = self.artifact_directory(
            context
                .capability
                .session_id
                .map_or("unscoped", |session| session.0.as_str()),
            &request.call_id.0,
        );
        let mut stdout_task =
            tokio::spawn(read_capped(stdout, artifact_directory.join("stdout.log")));
        let mut stderr_task =
            tokio::spawn(read_capped(stderr, artifact_directory.join("stderr.log")));
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
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                let _ = tokio::fs::remove_dir_all(&artifact_directory).await;
                return Err(match outcome {
                    Some(_) => ToolError::ExecutionFailed("shell command timed out".into()),
                    None => ToolError::Cancelled,
                });
            }
        };
        let mut output = json!({
            "exit_code": status.code(),
            "success": status.success(),
            "stdout": stdout.preview,
            "stderr": stderr.preview
        });
        if let Some(path) = stdout.artifact {
            output["stdout_artifact"] = json!(path.display().to_string());
        }
        if let Some(path) = stderr.artifact {
            output["stderr_artifact"] = json!(path.display().to_string());
        }
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(output),
        )))
    }
}

impl ShellTool {
    fn artifact_directory(&self, session_id: &str, call_id: &str) -> PathBuf {
        let root = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .map_or_else(
                || self.root.join(".kit/artifacts"),
                |home| home.join(".kit/artifacts"),
            );
        root.join(safe_component(session_id))
            .join(safe_component(call_id))
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

struct CapturedOutput {
    preview: String,
    artifact: Option<PathBuf>,
}

async fn read_capped(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    mut artifact_path: PathBuf,
) -> std::io::Result<CapturedOutput> {
    let mut small = Vec::new();
    let mut head = Vec::with_capacity(MAX_MODEL_OUTPUT_BYTES / 2);
    let mut tail = VecDeque::with_capacity(MAX_MODEL_OUTPUT_BYTES / 2);
    let mut artifact = None;
    let mut total = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        total = total.saturating_add(read as u64);
        retain_preview(&mut head, &mut tail, chunk);
        if artifact.is_none() && small.len() + read <= MAX_MODEL_OUTPUT_BYTES {
            small.extend_from_slice(chunk);
            continue;
        }
        if artifact.is_none() {
            let (mut file, path) = create_artifact(&artifact_path).await?;
            artifact_path = path;
            file.write_all(&small).await?;
            artifact = Some(file);
            small.clear();
        }
        if let Some(file) = &mut artifact {
            file.write_all(chunk).await?;
        }
    }
    let Some(mut artifact) = artifact else {
        return Ok(CapturedOutput {
            preview: String::from_utf8_lossy(&small).into_owned(),
            artifact: None,
        });
    };
    artifact.flush().await?;
    drop(artifact);
    let marker = format!("\n...[shell output spilled: {total} bytes; see artifact field]...\n");
    let remaining = MAX_MODEL_OUTPUT_BYTES.saturating_sub(marker.len());
    let head_budget = remaining / 2;
    let tail_budget = remaining - head_budget;
    let preview = format!(
        "{}{}{}",
        lossy_prefix(&head, head_budget),
        marker,
        lossy_suffix(tail.make_contiguous(), tail_budget)
    );
    Ok(CapturedOutput {
        preview,
        artifact: Some(artifact_path),
    })
}

async fn create_artifact(path: &Path) -> std::io::Result<(tokio::fs::File, PathBuf)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("log");
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            path.to_path_buf()
        } else {
            path.with_file_name(format!("{stem}-{attempt}.{extension}"))
        };
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&candidate).await {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no available artifact filename under {}", parent.display()),
    ))
}

fn retain_preview(head: &mut Vec<u8>, tail: &mut VecDeque<u8>, chunk: &[u8]) {
    let half = MAX_MODEL_OUTPUT_BYTES / 2;
    let missing = half.saturating_sub(head.len());
    head.extend_from_slice(&chunk[..chunk.len().min(missing)]);
    tail.extend(chunk);
    if tail.len() > half {
        tail.drain(..tail.len() - half);
    }
}

fn lossy_prefix(bytes: &[u8], budget: usize) -> String {
    let mut value = String::from_utf8_lossy(bytes).into_owned();
    while value.len() > budget || !value.is_char_boundary(value.len().min(budget)) {
        value.pop();
    }
    value
}

fn lossy_suffix(bytes: &[u8], budget: usize) -> String {
    let value = String::from_utf8_lossy(bytes);
    if value.len() <= budget {
        return value.into_owned();
    }
    let mut start = value.len() - budget;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

fn safe_component(value: &str) -> String {
    let prefix = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(48)
        .collect::<String>();
    let hash = blake3::hash(value.as_bytes()).to_hex().to_string();
    format!(
        "{}-{}",
        if prefix.is_empty() { "id" } else { &prefix },
        &hash[..12]
    )
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

    use tokio::io::AsyncWriteExt as _;

    use super::{
        MAX_MODEL_OUTPUT_BYTES, isolate_process_tree, kill_process_tree, read_capped, shell_command,
    };

    #[tokio::test]
    async fn oversized_output_spills_to_an_artifact_and_returns_a_bounded_preview() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("stdout.log");
        let data = [vec![b'A'; 10 * 1024], vec![b'Z'; 10 * 1024]].concat();
        let (mut writer, reader) = tokio::io::duplex(data.len() + 1);
        writer.write_all(&data).await.unwrap();
        writer.shutdown().await.unwrap();

        let captured = read_capped(reader, artifact.clone()).await.unwrap();

        assert_eq!(std::fs::read(&artifact).unwrap(), data);
        assert_eq!(captured.artifact.as_deref(), Some(artifact.as_path()));
        assert!(captured.preview.len() <= MAX_MODEL_OUTPUT_BYTES);
        assert!(captured.preview.starts_with("AAAA"));
        assert!(captured.preview.ends_with("ZZZZ"));
        assert!(
            captured
                .preview
                .contains("shell output spilled: 20480 bytes")
        );
    }

    #[tokio::test]
    async fn small_output_stays_inline_without_an_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("stdout.log");
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"small output").await.unwrap();
        writer.shutdown().await.unwrap();

        let captured = read_capped(reader, artifact.clone()).await.unwrap();

        assert_eq!(captured.preview, "small output");
        assert!(captured.artifact.is_none());
        assert!(!artifact.exists());
    }

    #[tokio::test]
    async fn spilling_does_not_overwrite_an_existing_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("stdout.log");
        std::fs::write(&artifact, b"existing").unwrap();
        let data = vec![b'X'; MAX_MODEL_OUTPUT_BYTES + 1];
        let (mut writer, reader) = tokio::io::duplex(data.len() + 1);
        writer.write_all(&data).await.unwrap();
        writer.shutdown().await.unwrap();

        let captured = read_capped(reader, artifact.clone()).await.unwrap();

        assert_eq!(std::fs::read(&artifact).unwrap(), b"existing");
        let spilled = captured.artifact.unwrap();
        assert_eq!(spilled.file_name().unwrap(), "stdout-1.log");
        assert_eq!(std::fs::read(spilled).unwrap(), data);
    }

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
