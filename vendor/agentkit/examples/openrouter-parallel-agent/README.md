# `openrouter-parallel-agent`

One-shot OpenRouter example that wires together:

- `agentkit-loop`
- `agentkit-provider-openrouter`
- `agentkit-task-manager` — `AsyncTaskManager` with a custom routing policy
- `agentkit-tool-fs`
- `agentkit-tool-shell`
- `agentkit-reporting`

This example demonstrates how the `AsyncTaskManager` schedules tool calls as async tasks with different routing strategies. A closure-based `TaskRoutingPolicy` routes `fs.*` calls as foreground (blocking the turn until resolved) and `shell_exec` calls as foreground-then-detach-after-2-seconds (auto-promoted to background if they haven't finished in time).

A side task listens to the `TaskManagerHandle` event stream and prints task lifecycle events (`started`, `detached`, `completed`, `cancelled`, `failed`) to stderr so you can observe the scheduling in real time.

## Run

Multi-file read — the model issues several foreground `fs_read_file` calls:

```bash
cargo run -p openrouter-parallel-agent -- \
  "List all Cargo.toml files in this workspace, then read each one and tell me the crate names."
```

Detach-after-timeout — the shell command exceeds the 2-second threshold and gets promoted to background:

```bash
cargo run -p openrouter-parallel-agent -- \
  "Run 'sleep 5 && echo done' in the shell."
```

Background script with a secret — the included `delayed-secret.sh` sleeps for 10 seconds then prints a secret value. The model can continue working while it runs:

```bash
cargo run -p openrouter-parallel-agent -- \
  "Run the delayed-secret script and tell me what secret it prints. While waiting, write me a haiku about Rust."
```

Quick shell command — completes within the foreground window:

```bash
cargo run -p openrouter-parallel-agent -- \
  "Count the number of .rs files in this repository using find and wc."
```

## Notes

- The example loads environment variables from the workspace `.env`.
- Filesystem tools are gated by a `PathPolicy` rooted at the current working directory.
- Shell tools are gated by a `CommandPolicy` that allows read-only commands: `pwd`, `ls`, `cat`, `find`, `wc`, `grep`, `echo`, `sleep`, and the bundled `delayed-secret.sh` script.
- Task lifecycle events are printed to stderr alongside the `StdoutReporter` output.
- The process waits for all background tasks to complete via `TaskManagerHandle::wait_for_idle()` before exiting.
