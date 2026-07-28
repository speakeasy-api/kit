# agentkit-tool-shell

<p align="center">
  <a href="https://crates.io/crates/agentkit-tool-shell"><img src="https://img.shields.io/crates/v/agentkit-tool-shell.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-tool-shell"><img src="https://img.shields.io/docsrs/agentkit-tool-shell?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-tool-shell.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Shell execution tool for running subprocesses inside an agentkit agent loop.

This crate provides `ShellExecTool`, registered under the name `shell_exec`.
When invoked it spawns a subprocess and returns structured JSON containing
`stdout`, `stderr`, `success`, and `exit_code`. The shape is declared on the
spec via `ToolSpec::output_schema` so hosts and composing tools can read it
directly.

Features:

- Execute any command with arguments
- Set a custom working directory and environment variables per invocation
- Per-command timeout via `timeout_ms`
- Cooperative turn cancellation -- the subprocess is killed when the turn is cancelled
- Emits `ShellPermissionRequest` so permission policies can gate execution

Pair it with `CommandPolicy` from `agentkit-tools-core` when you need fine-grained
control over which executables, working directories, and environment variables are
permitted.

## Quick start

Use `registry()` to get a `ToolRegistry` pre-loaded with the `shell_exec` tool:

```rust
use agentkit_tool_shell::registry;

let reg = registry();
let specs = reg.specs();
assert_eq!(specs.len(), 1);
assert_eq!(specs[0].name.0, "shell_exec");
```

## Constructing the tool manually

You can also create `ShellExecTool` directly and register it alongside other
tools:

```rust
use agentkit_tool_shell::ShellExecTool;
use agentkit_tools_core::ToolRegistry;

let reg = ToolRegistry::new()
    .with(ShellExecTool::default());

assert!(reg.specs().iter().any(|s| s.name.0 == "shell_exec"));
```

## Executing a command with `BasicToolExecutor`

The following example shows how to build a tool request, execute it through the
standard `BasicToolExecutor`, and inspect the result. A trivial "allow-all"
permission checker is used for brevity -- production code should use
`CommandPolicy` or a custom `PermissionChecker`.

```rust,no_run
use agentkit_tool_shell::registry;
use agentkit_tools_core::{
    BasicToolExecutor, PermissionChecker, PermissionDecision, PermissionRequest,
    ToolExecutionOutcome, ToolExecutor, ToolContext, ToolName, ToolRequest,
};
use agentkit_capabilities::CapabilityContext;
use agentkit_core::{MetadataMap, SessionId, ToolOutput, TurnId};
use serde_json::json;

struct AllowAll;
impl PermissionChecker for AllowAll {
    fn evaluate(&self, _req: &dyn PermissionRequest) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

#[tokio::main]
async fn main() {
    let executor = BasicToolExecutor::from_registry(registry());
    let metadata = MetadataMap::new();
    let mut ctx = ToolContext {
        capability: CapabilityContext {
            session_id: Some(&SessionId::new("s1")),
            turn_id: Some(&TurnId::new("t1")),
            metadata: &metadata,
        },
        permissions: &AllowAll,
        resources: &(),
        cancellation: None,
    };

    let request = ToolRequest {
        call_id: "call-1".into(),
        tool_name: ToolName::new("shell_exec"),
        input: json!({
            "executable": "echo",
            "argv": ["hello", "world"],
            "timeout_ms": 5000
        }),
        session_id: "s1".into(),
        turn_id: "t1".into(),
        metadata: MetadataMap::new(),
    };

    match executor.execute(request, &mut ctx).await {
        ToolExecutionOutcome::Completed(result) => {
            if let ToolOutput::Structured(value) = result.result.output {
                println!("stdout: {}", value["stdout"]);
                println!("exit_code: {}", value["exit_code"]);
            }
        }
        ToolExecutionOutcome::Failed(err) => eprintln!("tool error: {err}"),
        other => eprintln!("unexpected outcome: {other:?}"),
    }
}
```

## Restricting commands with `CommandPolicy`

```rust
use agentkit_tools_core::{CommandPolicy, CompositePermissionChecker, PermissionDecision};

let permissions = CompositePermissionChecker::new(PermissionDecision::Allow)
    .with_policy(
        CommandPolicy::new()
            .allow_executable("git")
            .allow_executable("cargo")
            .deny_executable("rm")
            .require_approval_for_unknown(true),
    );
```
