//! Shell execution tool for agentkit agent loops.
//!
//! This crate provides [`ShellExecTool`], a tool that spawns subprocesses and
//! captures their stdout, stderr, exit code, and success status.  It supports
//! custom working directories, environment variables, per-invocation timeouts,
//! and cooperative turn cancellation through [`agentkit_tools_core::ToolContext`].
//!
//! The easiest way to get started is with the [`registry()`] helper, which
//! returns a [`ToolRegistry`] pre-loaded with the `shell_exec` tool.
//!
//! Pair the tool with [`CommandPolicy`](agentkit_tools_core::CommandPolicy) from
//! `agentkit-tools-core` when you need fine-grained control over which
//! executables, working directories, and environment variables are permitted.
//!
//! # Example
//!
//! ```rust
//! use agentkit_tool_shell::{registry, ShellExecTool};
//! use agentkit_tools_core::Tool;
//!
//! // Build a registry that contains the shell_exec tool.
//! let reg = registry();
//! let specs = reg.specs();
//! assert!(specs.iter().any(|s| s.name.0 == "shell_exec"));
//!
//! // Or construct the tool manually and register it yourself.
//! let tool = ShellExecTool::default();
//! assert_eq!(tool.spec().name.0, "shell_exec");
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agentkit_core::{MetadataMap, ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    PermissionRequest, ShellPermissionRequest, Tool, ToolAnnotations, ToolContext, ToolError,
    ToolName, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;
use tokio::time::timeout;

/// Creates a [`ToolRegistry`] pre-populated with [`ShellExecTool`].
///
/// This is the simplest way to add shell execution to an agent.  The returned
/// registry contains a single tool registered under the name `shell_exec`.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_shell::registry;
///
/// let reg = registry();
/// assert_eq!(reg.specs().len(), 1);
/// assert_eq!(reg.specs()[0].name.0, "shell_exec");
/// ```
pub fn registry() -> ToolRegistry {
    ToolRegistry::new().with(ShellExecTool::default())
}

/// A tool that executes shell commands as subprocesses.
///
/// `ShellExecTool` implements the [`Tool`] trait and is registered under the
/// name `shell_exec`.  When invoked it spawns the requested executable, waits
/// for it to finish (respecting an optional timeout and turn cancellation), and
/// returns a structured JSON object with `stdout`, `stderr`, `success`, and
/// `exit_code` fields.
///
/// Before execution the tool emits a [`ShellPermissionRequest`] so that
/// permission policies (e.g. [`CommandPolicy`](agentkit_tools_core::CommandPolicy))
/// can allow, deny, or require approval for the command.
///
/// # Input schema
///
/// | Field          | Type              | Required | Description                              |
/// |----------------|-------------------|----------|------------------------------------------|
/// | `executable`   | `string`          | yes      | Program to run.                          |
/// | `argv`         | `[string]`        | no       | Arguments passed to the executable.      |
/// | `cwd`          | `string`          | no       | Working directory for the subprocess.    |
/// | `env`          | `{string:string}` | no       | Extra environment variables.             |
/// | `timeout_ms`   | `integer`         | no       | Maximum wall-clock time in milliseconds. |
///
/// # Example
///
/// ```rust
/// use agentkit_tool_shell::ShellExecTool;
/// use agentkit_tools_core::ToolRegistry;
///
/// let mut reg = ToolRegistry::new();
/// reg.register(ShellExecTool::default());
///
/// let spec = &reg.specs()[0];
/// assert_eq!(spec.name.0, "shell_exec");
/// assert!(spec.annotations.destructive_hint);
/// ```
#[derive(Clone, Debug)]
pub struct ShellExecTool {
    spec: ToolSpec,
}

impl Default for ShellExecTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("shell_exec"),
                description: "Execute a shell command and capture stdout, stderr, and exit status."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "executable": { "type": "string" },
                        "argv": {
                            "type": "array",
                            "items": { "type": "string" },
                            "default": []
                        },
                        "cwd": { "type": "string" },
                        "env": {
                            "type": "object",
                            "additionalProperties": { "type": "string" }
                        },
                        "timeout_ms": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["executable"],
                    "additionalProperties": false
                }),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "stdout": { "type": "string" },
                        "stderr": { "type": "string" },
                        "success": { "type": "boolean" },
                        "exit_code": { "type": ["integer", "null"], "description": "null when the process was terminated by a signal" }
                    },
                    "required": ["stdout", "stderr", "success"]
                })),
                annotations: ToolAnnotations {
                    destructive_hint: true,
                    needs_approval_hint: true,
                    ..ToolAnnotations::default()
                },
                metadata: MetadataMap::new(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct ShellExecInput {
    executable: String,
    #[serde(default)]
    argv: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for ShellExecTool {
    /// Returns the [`ToolSpec`] describing the `shell_exec` tool.
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// Extracts a [`ShellPermissionRequest`] from the incoming [`ToolRequest`].
    ///
    /// The returned request is evaluated by the active
    /// [`PermissionChecker`](agentkit_tools_core::PermissionChecker) before
    /// [`invoke`](Self::invoke) runs, giving policies such as
    /// [`CommandPolicy`](agentkit_tools_core::CommandPolicy) a chance to allow
    /// or deny the command.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] if the request input cannot be
    /// deserialized into the expected schema.
    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let input: ShellExecInput = parse_input(request)?;
        Ok(vec![Box::new(ShellPermissionRequest {
            executable: input.executable,
            argv: input.argv,
            cwd: input.cwd,
            env_keys: input.env.keys().cloned().collect(),
            metadata: request.metadata.clone(),
        })])
    }

    /// Spawns the requested command and returns its output.
    ///
    /// The subprocess is spawned with `kill_on_drop(true)` so it is cleaned up
    /// if the future is cancelled.  When a `timeout_ms` is specified in the
    /// input the command is aborted after that duration.  If a turn
    /// cancellation token is present in the [`ToolContext`] the command is also
    /// aborted when the turn is cancelled.
    ///
    /// On success the returned [`ToolResult`] contains a JSON object:
    ///
    /// ```json
    /// {
    ///   "stdout": "...",
    ///   "stderr": "...",
    ///   "success": true,
    ///   "exit_code": 0
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// * [`ToolError::InvalidInput`] -- the request input does not match the schema.
    /// * [`ToolError::ExecutionFailed`] -- the command could not be spawned or timed out.
    /// * [`ToolError::Cancelled`] -- the turn was cancelled while the command was running.
    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: ShellExecInput = parse_input(&request)?;
        let mut command = Command::new(&input.executable);
        command.args(&input.argv);
        command.kill_on_drop(true);
        if let Some(cwd) = &input.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &input.env {
            command.env(key, value);
        }

        let duration_start = std::time::Instant::now();
        let output_future = command.output();
        tokio::pin!(output_future);

        let output = if let Some(timeout_ms) = input.timeout_ms {
            if let Some(cancellation) = ctx.cancellation.clone() {
                tokio::select! {
                    result = &mut output_future => result.map_err(|error| {
                        ToolError::ExecutionFailed(format!("failed to spawn command: {error}"))
                    })?,
                    _ = cancellation.cancelled() => return Err(ToolError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                        return Err(ToolError::ExecutionFailed(format!("command timed out after {timeout_ms}ms")));
                    }
                }
            } else {
                timeout(Duration::from_millis(timeout_ms), &mut output_future)
                    .await
                    .map_err(|_| {
                        ToolError::ExecutionFailed(format!(
                            "command timed out after {timeout_ms}ms"
                        ))
                    })?
                    .map_err(|error| {
                        ToolError::ExecutionFailed(format!("failed to spawn command: {error}"))
                    })?
            }
        } else if let Some(cancellation) = ctx.cancellation.clone() {
            tokio::select! {
                result = &mut output_future => result.map_err(|error| {
                    ToolError::ExecutionFailed(format!("failed to spawn command: {error}"))
                })?,
                _ = cancellation.cancelled() => return Err(ToolError::Cancelled),
            }
        } else {
            output_future.await.map_err(|error| {
                ToolError::ExecutionFailed(format!("failed to spawn command: {error}"))
            })?
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let status = output.status.code();
        let success = output.status.success();

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Structured(json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "success": success,
                    "exit_code": status,
                })),
                is_error: !success,
                metadata: MetadataMap::new(),
            },
            duration: Some(duration_start.elapsed()),
            metadata: MetadataMap::new(),
        })
    }
}

fn parse_input(request: &ToolRequest) -> Result<ShellExecInput, ToolError> {
    serde_json::from_value(request.input.clone())
        .map_err(|error| ToolError::InvalidInput(format!("invalid tool input: {error}")))
}

#[cfg(test)]
mod tests {
    use agentkit_capabilities::CapabilityContext;
    use agentkit_core::{SessionId, TurnId};
    use agentkit_tools_core::{
        BasicToolExecutor, PermissionChecker, PermissionCode, PermissionDecision, PermissionDenial,
        ToolExecutionOutcome, ToolExecutor,
    };

    use super::*;

    struct AllowAll;

    impl PermissionChecker for AllowAll {
        fn evaluate(
            &self,
            _request: &dyn agentkit_tools_core::PermissionRequest,
        ) -> PermissionDecision {
            PermissionDecision::Allow
        }
    }

    struct DenyCommands;

    impl PermissionChecker for DenyCommands {
        fn evaluate(
            &self,
            _request: &dyn agentkit_tools_core::PermissionRequest,
        ) -> PermissionDecision {
            PermissionDecision::Deny(PermissionDenial {
                code: PermissionCode::CommandNotAllowed,
                message: "commands denied in test".into(),
                metadata: MetadataMap::new(),
            })
        }
    }

    #[tokio::test]
    async fn shell_tool_executes_and_captures_output() {
        let executor = BasicToolExecutor::from_registry(registry());
        let metadata = MetadataMap::new();
        let mut ctx = ToolContext {
            capability: CapabilityContext {
                session_id: Some(&SessionId::new("session-1")),
                turn_id: Some(&TurnId::new("turn-1")),
                metadata: &metadata,
            },
            permissions: &AllowAll,
            resources: &(),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };

        let result = executor
            .execute(
                ToolRequest {
                    call_id: "call-1".into(),
                    tool_name: ToolName::new("shell_exec"),
                    input: json!({
                        "executable": "sh",
                        "argv": ["-c", "printf hello"]
                    }),
                    session_id: "session-1".into(),
                    turn_id: "turn-1".into(),
                    metadata: MetadataMap::new(),
                },
                &mut ctx,
            )
            .await;

        match result {
            ToolExecutionOutcome::Completed(result) => {
                let value = match result.result.output {
                    ToolOutput::Structured(value) => value,
                    other => panic!("unexpected output: {other:?}"),
                };
                assert_eq!(value["stdout"], "hello");
                assert_eq!(value["success"], true);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_tool_respects_permission_denial() {
        let executor = BasicToolExecutor::from_registry(registry());
        let metadata = MetadataMap::new();
        let mut ctx = ToolContext {
            capability: CapabilityContext {
                session_id: Some(&SessionId::new("session-1")),
                turn_id: Some(&TurnId::new("turn-1")),
                metadata: &metadata,
            },
            permissions: &DenyCommands,
            resources: &(),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };

        let result = executor
            .execute(
                ToolRequest {
                    call_id: "call-2".into(),
                    tool_name: ToolName::new("shell_exec"),
                    input: json!({
                        "executable": "sh",
                        "argv": ["-c", "printf nope"]
                    }),
                    session_id: "session-1".into(),
                    turn_id: "turn-1".into(),
                    metadata: MetadataMap::new(),
                },
                &mut ctx,
            )
            .await;

        assert!(matches!(
            result,
            ToolExecutionOutcome::FailedBeforeInvocation(ToolError::PermissionDenied(_))
        ));
    }
}
