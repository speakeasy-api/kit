use std::{path::PathBuf, process::Stdio, time::Duration};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use futures_util::{FutureExt as _, StreamExt as _, stream};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::{io::AsyncReadExt, process::Command};

use crate::process_tree::{isolate_tokio_process_tree, terminate_tokio_process_tree};
use crate::protocols::acp::tool_projection::terminal::{Output, Terminal};

const MAX_INTERNAL_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct ShellTool {
    root: PathBuf,
    spec: ToolSpec,
}

impl ShellTool {
    pub fn new(root: PathBuf) -> Self {
        let input_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "properties".into(),
                Value::Object(Map::from_iter([
                    (
                        "command".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                    (
                        "timeout_seconds".into(),
                        Value::Object(Map::from_iter([
                            ("type".into(), Value::from("integer")),
                            ("minimum".into(), Value::from(1)),
                            ("maximum".into(), Value::from(3600)),
                            ("default".into(), Value::from(120)),
                        ])),
                    ),
                ])),
            ),
            (
                "required".into(),
                Value::Array(vec![Value::from("command")]),
            ),
            ("additionalProperties".into(), Value::from(false)),
        ]));
        let output_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "properties".into(),
                Value::Object(Map::from_iter([
                    (
                        "exit_code".into(),
                        Value::Object(Map::from_iter([(
                            "type".into(),
                            Value::Array(vec![Value::from("integer"), Value::from("null")]),
                        )])),
                    ),
                    (
                        "success".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("boolean"))])),
                    ),
                    (
                        "stdout".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                    (
                        "stderr".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                ])),
            ),
            (
                "required".into(),
                Value::Array(vec![
                    Value::from("exit_code"),
                    Value::from("success"),
                    Value::from("stdout"),
                    Value::from("stderr"),
                ]),
            ),
            ("additionalProperties".into(), Value::from(false)),
        ]));
        Self {
            root,
            spec: ToolSpec::new(
                ToolName::new("shell"),
                "Run a shell command from Kit's working directory. Commands can access any path allowed to the Kit process.",
                input_schema,
            )
            .with_output_schema(output_schema)
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
        mut request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let cancellation = context.cancellation.clone();
        let input: ShellInput = serde_json::from_value(std::mem::take(&mut request.input))
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if input.command.is_empty() || !(1..=3600).contains(&input.timeout_seconds) {
            return Err(ToolError::InvalidInput(
                "command and timeout_seconds are outside bounds".into(),
            ));
        }
        let mut command = shell_command(&input.command)?;
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
        let terminal = Terminal::start(&request, &input.command, &self.root);
        let mut stdout_task = tokio::spawn(read_output(stdout, terminal.output()));
        let mut stderr_task = tokio::spawn(read_output(stderr, terminal.output()));
        let mut stdout_finished = false;
        let mut stderr_finished = false;
        let mut status = None;
        let mut stdout = None;
        let mut stderr = None;
        let timeout = tokio::time::sleep(Duration::from_secs(input.timeout_seconds));
        // A cancelled turn must not wait out the command: the loop awaits this
        // invocation, so an uncooperative tool keeps the whole turn alive until
        // the timeout, however long the caller asked for.
        let interrupted = async {
            match &cancellation {
                Some(cancellation) => cancellation.cancelled().await,
                None => std::future::pending().await,
            }
        };

        let outcome = {
            // Keep one merge alive throughout collection. Each source yields
            // once and then exhausts, replacing select guards without polling
            // completed wait/join futures again or rebuilding a priority race.
            let mut events = stream::SelectAll::from_iter([
                stream::once(child.wait().map(ShellEvent::Wait)).boxed(),
                stream::once((&mut stdout_task).map(ShellEvent::Stdout)).boxed(),
                stream::once((&mut stderr_task).map(ShellEvent::Stderr)).boxed(),
                stream::once(timeout.map(|()| ShellEvent::Timeout)).boxed(),
                stream::once(interrupted.map(|()| ShellEvent::Cancelled)).boxed(),
            ]);
            loop {
                if status.is_some() && stdout.is_some() && stderr.is_some() {
                    break Ok(());
                }
                let Some(event) = events.next().await else {
                    break Err(ToolError::Internal(
                        "shell events exhausted before collection".into(),
                    ));
                };
                match event {
                    ShellEvent::Wait(Ok(exit_status)) => status = Some(exit_status),
                    ShellEvent::Wait(Err(error)) => {
                        break Err(ToolError::ExecutionFailed(error.to_string()));
                    }
                    ShellEvent::Stdout(result) => {
                        stdout_finished = true;
                        match output_task_result(result) {
                            Ok(output) => stdout = Some(output),
                            Err(error) => break Err(error),
                        }
                    }
                    ShellEvent::Stderr(result) => {
                        stderr_finished = true;
                        match output_task_result(result) {
                            Ok(output) => stderr = Some(output),
                            Err(error) => break Err(error),
                        }
                    }
                    ShellEvent::Timeout => {
                        break Err(ToolError::ExecutionFailed("shell command timed out".into()));
                    }
                    ShellEvent::Cancelled => break Err(ToolError::Cancelled),
                }
            }
        };
        // The merge and its child/handle borrows are gone before cleanup.
        // Dropping join handles only detaches: terminate the process tree, then
        // abort and await every reader whose result was not already consumed.
        if let Err(error) = outcome {
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
        let status =
            status.ok_or_else(|| ToolError::Internal("shell status was not collected".into()))?;
        let stdout =
            stdout.ok_or_else(|| ToolError::Internal("shell stdout was not collected".into()))?;
        let stderr =
            stderr.ok_or_else(|| ToolError::Internal("shell stderr was not collected".into()))?;
        terminal.finish(status.code());
        let output = Value::Object(Map::from_iter([
            (
                "exit_code".into(),
                status.code().map_or(Value::Null, Value::from),
            ),
            ("success".into(), Value::from(status.success())),
            ("stdout".into(), Value::from(stdout)),
            ("stderr".into(), Value::from(stderr)),
        ]));
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

async fn read_output(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    output: Option<Output>,
) -> std::io::Result<String> {
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
        if let Some(output) = &output {
            output.chunk(&buffer[..read]);
        }
        content.extend_from_slice(&buffer[..read]);
    }
    Ok(String::from_utf8(content)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
}

#[cfg(unix)]
fn shell_command(input: &str) -> Result<Command, ToolError> {
    let explicit = std::env::var_os("KIT_SHELL");
    let environment = std::env::var_os("SHELL");
    let shell = resolve_shell(
        explicit.as_deref(),
        environment.as_deref(),
        account_login_shell,
    )?;
    let interactive = std::env::var_os("KIT_SHELL_INTERACTIVE");
    Ok(unix_shell_command(&shell, interactive.as_deref(), input))
}

#[cfg(unix)]
fn unix_shell_command(
    shell: &std::path::Path,
    interactive: Option<&std::ffi::OsStr>,
    input: &str,
) -> Command {
    let mut command = Command::new(shell);
    command
        .arg(if interactive == Some(std::ffi::OsStr::new("1")) {
            "-lic"
        } else {
            "-lc"
        })
        .arg(input);
    command
}

#[cfg(unix)]
fn resolve_shell(
    explicit: Option<&std::ffi::OsStr>,
    environment: Option<&std::ffi::OsStr>,
    account: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf, ToolError> {
    if let Some(explicit) = explicit {
        let path = std::path::Path::new(explicit);
        if !is_executable_shell(path) {
            return Err(ToolError::ExecutionFailed(format!(
                "invalid KIT_SHELL {:?}: expected an absolute path to an executable file",
                path
            )));
        }
        return Ok(path.to_path_buf());
    }
    Ok(environment
        .map(PathBuf::from)
        .filter(|path| is_automatic_shell(path))
        .or_else(|| account().filter(|path| is_automatic_shell(path)))
        .unwrap_or_else(|| PathBuf::from("/bin/sh")))
}

#[cfg(unix)]
fn is_automatic_shell(path: &std::path::Path) -> bool {
    // Only infer shells with our -lc/-lic invocation contract. Unknown shells
    // and wrappers remain available through an explicit KIT_SHELL override.
    matches!(
        path.file_name().and_then(std::ffi::OsStr::to_str),
        Some("sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" | "mksh" | "fish")
    ) && is_executable_shell(path)
}

#[cfg(unix)]
fn is_executable_shell(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    if !path.is_absolute() || !path.is_file() {
        return false;
    }
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: path is a live, NUL-terminated C string.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(unix)]
fn account_login_shell() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    // Use caller-owned storage rather than getpwuid's shared static buffer.
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all output pointers refer to live, correctly sized storage.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        // SAFETY: success with a non-null result initialized entry.
        let entry = unsafe { entry.assume_init() };
        if entry.pw_shell.is_null() {
            return None;
        }
        // SAFETY: the NUL-terminated string lives in buffer until it is copied.
        let shell = unsafe { std::ffi::CStr::from_ptr(entry.pw_shell) };
        return Some(PathBuf::from(std::ffi::OsStr::from_bytes(shell.to_bytes())));
    }
}

#[cfg(windows)]
fn shell_command(input: &str) -> Result<Command, ToolError> {
    let mut command = Command::new("cmd");
    command.arg("/C").arg(input);
    Ok(command)
}

const fn default_timeout() -> u64 {
    120
}

#[cfg(all(test, unix))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use agentkit_core::{
        CancellationController, MetadataMap, SessionId, ToolOutput, TurnCancellation, TurnId,
    };
    use agentkit_tools_core::{
        AllowAllPermissions, OwnedToolContext, Tool as _, ToolError, ToolRequest, ToolResult,
    };
    use serde_json::json;
    use tokio::io::AsyncWriteExt as _;

    use super::{MAX_INTERNAL_OUTPUT_BYTES, ShellTool, read_output};

    #[test]
    fn shell_resolution_priority_and_invalid_candidates() {
        use std::{ffi::OsStr, os::unix::fs::PermissionsExt as _, path::PathBuf};

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("bash");
        let second = directory.path().join("zsh");
        for path in [&first, &second] {
            std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        fn resolve(
            explicit: Option<&OsStr>,
            environment: Option<&OsStr>,
            account: Option<PathBuf>,
        ) -> Result<PathBuf, ToolError> {
            super::resolve_shell(explicit, environment, || account)
        }
        assert_eq!(
            resolve(Some(first.as_os_str()), Some(second.as_os_str()), None).unwrap(),
            first
        );
        assert_eq!(
            resolve(None, Some(first.as_os_str()), Some(second.clone())).unwrap(),
            first
        );
        assert_eq!(
            resolve(None, Some(OsStr::new("relative")), Some(second.clone())).unwrap(),
            second
        );
        assert_eq!(resolve(None, None, Some(second.clone())).unwrap(), second);
        assert_eq!(resolve(None, None, None).unwrap(), PathBuf::from("/bin/sh"));
        assert_eq!(
            resolve(None, Some(OsStr::new("")), Some(PathBuf::from("relative"))).unwrap(),
            PathBuf::from("/bin/sh")
        );

        for name in ["tcsh", "csh", "unknown-shell"] {
            let incompatible = directory.path().join(name);
            std::fs::write(&incompatible, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&incompatible, std::fs::Permissions::from_mode(0o700))
                .unwrap();
            assert_eq!(
                resolve(None, Some(incompatible.as_os_str()), Some(second.clone())).unwrap(),
                second
            );
            assert_eq!(
                resolve(None, None, Some(incompatible.clone())).unwrap(),
                PathBuf::from("/bin/sh")
            );
            // Explicit overrides can still select custom shell wrappers.
            assert_eq!(
                resolve(Some(incompatible.as_os_str()), None, None).unwrap(),
                incompatible
            );
        }

        let nonexecutable = directory.path().join("not executable");
        std::fs::write(&nonexecutable, "no").unwrap();
        std::fs::set_permissions(&nonexecutable, std::fs::Permissions::from_mode(0o600)).unwrap();
        let missing = directory.path().join("missing");
        for invalid in [
            OsStr::new(""),
            OsStr::new("sh"),
            directory.path().as_os_str(),
            nonexecutable.as_os_str(),
            missing.as_os_str(),
        ] {
            let error =
                resolve(Some(invalid), Some(first.as_os_str()), Some(second.clone())).unwrap_err();
            assert!(error.to_string().contains("invalid KIT_SHELL"));
            assert_eq!(
                resolve(None, Some(invalid), Some(second.clone())).unwrap(),
                second
            );
            assert_eq!(
                resolve(None, None, Some(PathBuf::from(invalid))).unwrap(),
                PathBuf::from("/bin/sh")
            );
        }
    }

    #[tokio::test]
    async fn selected_shell_executes_with_opt_in_interactive_mode() {
        use std::{ffi::OsStr, os::unix::fs::PermissionsExt as _};

        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("selected shell");
        // A real executable wrapper records the received flag, then starts a
        // real shell so the test also checks interactive behavior, not just argv.
        std::fs::write(
            &shell,
            "#!/bin/sh\nprintf 'selected:%s\\n' \"$1\"\nexec /bin/sh \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let selected = super::resolve_shell(Some(shell.as_os_str()), None, || None).unwrap();
        for (value, flag, mode) in [
            (None, "-lc", "noninteractive"),
            (Some(OsStr::new("0")), "-lc", "noninteractive"),
            (Some(OsStr::new("true")), "-lc", "noninteractive"),
            (Some(OsStr::new("1")), "-lic", "interactive"),
        ] {
            let output = super::unix_shell_command(
                &selected,
                value,
                "case $- in *i*) printf 'mode:interactive\\n';; *) printf 'mode:noninteractive\\n';; esac; exit 7",
            )
            .env_clear()
            .env("HOME", directory.path())
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap();
            assert_eq!(output.status.code(), Some(7));
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.contains(&format!("selected:{flag}\n")), "{stdout}");
            assert!(stdout.contains(&format!("mode:{mode}\n")), "{stdout}");
        }
    }

    #[test]
    fn account_login_shell_returns_owned_paths_across_concurrent_lookups() {
        let expected = super::account_login_shell();
        let lookups: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(super::account_login_shell))
            .collect();
        for lookup in lookups {
            // Returned paths outlive the lookup's local passwd storage and thread.
            assert_eq!(lookup.join().unwrap(), expected);
        }
        // Account records need not exist (for example, an unmapped container UID).
        let selected = super::resolve_shell(None, None, || expected.clone()).unwrap();
        assert!(selected.is_absolute());
        if let Some(path) = expected.filter(|path| super::is_automatic_shell(path)) {
            assert_eq!(selected, path);
        } else {
            assert_eq!(selected, std::path::Path::new("/bin/sh"));
        }
    }

    #[tokio::test]
    async fn zsh_interactive_startup_selects_fixture_runtime() {
        use std::{ffi::OsStr, os::unix::fs::PermissionsExt as _, path::Path};

        let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
            .into_iter()
            .map(Path::new)
            .find(|path| super::is_executable_shell(path))
        else {
            #[cfg(target_os = "macos")]
            panic!("macOS must provide zsh");
            #[cfg(not(target_os = "macos"))]
            {
                eprintln!("skipping zsh startup fixture: zsh is not installed");
                return;
            }
        };
        let home = tempfile::tempdir().unwrap();
        for (directory, version) in [
            ("login-bin", "login-runtime"),
            ("interactive-bin", "interactive-runtime"),
        ] {
            let bin = home.path().join(directory);
            std::fs::create_dir(&bin).unwrap();
            let runtime = bin.join("kit-fixture-runtime");
            std::fs::write(&runtime, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
            std::fs::set_permissions(runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // ZDOTDIR and HOME keep this separate from the user's startup files.
        // Disable later system-wide startup files after zsh's mandatory zshenv.
        std::fs::write(home.path().join(".zshenv"), "unsetopt GLOBAL_RCS\n").unwrap();
        std::fs::write(
            home.path().join(".zprofile"),
            "export PATH=\"$HOME/login-bin:/usr/bin:/bin\"\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join(".zshrc"),
            "export PATH=\"$HOME/interactive-bin:$PATH\"\n",
        )
        .unwrap();
        let selected = super::resolve_shell(Some(zsh.as_os_str()), None, || None).unwrap();
        for (interactive, bin, version) in [
            (None, "login-bin", "login-runtime"),
            (
                Some(OsStr::new("1")),
                "interactive-bin",
                "interactive-runtime",
            ),
        ] {
            let output = super::unix_shell_command(
                &selected,
                interactive,
                "command -v kit-fixture-runtime; kit-fixture-runtime",
            )
            .env_clear()
            .env("HOME", home.path())
            .env("ZDOTDIR", home.path())
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!(
                    "{}/kit-fixture-runtime\n{version}\n",
                    home.path().join(bin).display()
                )
            );
        }
    }

    async fn invoke_shell(
        root: &std::path::Path,
        command: &str,
        timeout_seconds: u64,
    ) -> Result<ToolResult, ToolError> {
        invoke_shell_with_cancellation(root, command, timeout_seconds, None).await
    }

    async fn invoke_shell_with_cancellation(
        root: &std::path::Path,
        command: &str,
        timeout_seconds: u64,
        cancellation: Option<TurnCancellation>,
    ) -> Result<ToolResult, ToolError> {
        let tool = ShellTool::new(root.to_path_buf());
        let context = OwnedToolContext {
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn"),
            metadata: MetadataMap::new(),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation,
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
        tool.invoke(request, &mut context.borrowed()).await
    }

    #[tokio::test]
    async fn structured_output_preserves_exit_codes_and_signal_null() {
        let directory = tempfile::tempdir().unwrap();
        for (command, expected) in [
            (
                "printf 'héllo'; printf 'error' >&2; exit 7",
                json!({"exit_code": 7, "success": false, "stdout": "héllo", "stderr": "error"}),
            ),
            (
                "exit 0",
                json!({"exit_code": 0, "success": true, "stdout": "", "stderr": ""}),
            ),
            (
                "kill -TERM $$",
                json!({"exit_code": null, "success": false, "stdout": "", "stderr": ""}),
            ),
        ] {
            let result = invoke_shell(directory.path(), command, 5).await.unwrap();
            let ToolOutput::Structured(output) = result.result.output else {
                panic!("structured output expected");
            };
            assert_eq!(output, expected);
        }
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

        let captured = read_output(reader, None).await.unwrap();

        assert_eq!(captured.as_bytes(), data);
    }

    #[tokio::test]
    async fn small_output_stays_complete() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"small output").await.unwrap();
        writer.shutdown().await.unwrap();

        let captured = read_output(reader, None).await.unwrap();

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
        let command = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let controller = CancellationController::new();
        let invocation = invoke_shell_with_cancellation(
            directory.path(),
            &command,
            30,
            Some(controller.handle().checkpoint()),
        );
        let interrupt = async {
            let descendant = tokio::time::timeout(Duration::from_secs(5), async {
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
            controller.interrupt();
            descendant
        };
        let (result, descendant) = tokio::time::timeout(
            Duration::from_secs(10),
            futures_util::future::join(invocation, interrupt),
        )
        .await
        .expect("cancellation did not stop the shell invocation");

        assert!(matches!(result, Err(ToolError::Cancelled)));
        assert_process_exited(descendant).await;
    }

    #[tokio::test]
    async fn closed_output_pipes_do_not_disable_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            invoke_shell(directory.path(), "exec 1>&- 2>&-; sleep 30", 1),
        )
        .await
        .expect("finished output readers disabled the timeout")
        .unwrap_err();
        assert!(
            matches!(error, ToolError::ExecutionFailed(message) if message == "shell command timed out")
        );
    }
}
