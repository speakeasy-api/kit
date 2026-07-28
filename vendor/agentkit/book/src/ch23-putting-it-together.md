# Putting it all together

This final chapter traces the complete path of a user request through the system, from keystroke to completed turn, touching every layer we've covered.

This is the payoff chapter. Every type, trait, and design decision from the previous 22 chapters appears here in context. If something below is unfamiliar, the cross-reference tells you where to look.

## The scenario

A user types: _"Add error handling to the parse function in src/parser.rs"_

The agent is configured with filesystem tools, shell tools, a `PathPolicy` for the workspace, a `CommandPolicy` with `cargo` in the allowlist, `ForegroundThenDetachAfter(10s)` for shell commands, a `KeepRecentStrategy` compaction pipeline, and a `CompositeReporter` writing to stdout and a usage tracker.

## What happens

### 1. Input submission

The CLI reads the user's message. The system prompt and any context items were preloaded via `AgentBuilder::transcript`; the very first user turn was preloaded via `AgentBuilder::input` so the opening `next()` dispatches the model directly. Subsequent user messages flow through the `InputRequest` handle the driver yields on each `AwaitingInput` interrupt:

```rust
input_request.submit(&mut driver, vec![Item::text(ItemKind::User, user_input)])?;
```

### 2. Compaction check

The driver checks the compaction trigger. If the transcript exceeds the configured threshold, the compaction pipeline runs — dropping old reasoning blocks, trimming failed tool results, keeping recent items.

### 3. Turn construction

The driver builds a `TurnRequest` containing:

- The working transcript (system prompt, context items, conversation history)
- Tool specs from the registry (fs_read_file, fs_write_file, fs_replace_in_file, shell_exec, etc.)
- The normalized prompt cache request for the turn

### 4. Model invocation

The adapter serializes the request and sends it to the provider. The response streams back as SSE chunks.

### 5. First tool call — read the file

The model decides it needs to see the file first. It emits a `ToolCallPart`:

```json
{ "name": "fs_read_file", "input": { "path": "src/parser.rs" } }
```

The driver:

1. Looks up `fs_read_file` in the registry
2. Evaluates the `FileSystemPermissionRequest::Read` against the permission checker
3. The `PathPolicy` allows reads under the workspace root → `Allow`
4. Executes the tool
5. `FileSystemToolResources` records that `src/parser.rs` has been read
6. Appends the `ToolResultPart` to the transcript

### 6. Automatic roundtrip

The driver starts another model turn with the updated transcript. The model now has the file contents.

### 7. Second tool call — edit the file

The model emits a `fs_replace_in_file` call with the old and new text.

The driver:

1. Evaluates `FileSystemPermissionRequest::Edit` for `src/parser.rs`
2. Checks read-before-write policy → the file was read in step 5 → `Allow`
3. Executes the replacement
4. Appends the result

### 8. Third tool call — verify the change

The model runs `shell_exec` with `cargo check`:

The driver:

1. Evaluates the `ShellPermissionRequest`
2. `CommandPolicy` has `cargo` in the allowlist → `Allow`
3. The task manager routes it as `ForegroundThenDetachAfter(10s)`
4. The command finishes in 3 seconds → result returned immediately
5. Appends the result

### 9. Final response

The model sees the successful build output and produces a text response explaining what it changed. The `StdoutReporter` streams each text chunk to the terminal as it arrives.

### 10. Turn completion

The model finishes with `FinishReason::Completed`. The driver returns `LoopStep::Finished(TurnResult)`. The CLI displays the usage summary and waits for the next user input.

## The dependency graph in action

```text
User input
  │
  ▼
agentkit-core ──────────── Item, Part, Delta, Usage, FinishReason, identifiers
  │
  ▼
agentkit-loop ──────────── LoopDriver, TurnRequest, LoopStep, AgentEvent
  │
  ├── agentkit-compaction ─ Compactor, StrategyCompactor, CompactionPipeline
  │                         (LoopMutator at AfterToolResult / AfterTurnEnded)
  │
  ├── agentkit-provider-* ─ ModelAdapter → ModelSession → ModelTurn
  │                         (step 4, sends transcript, streams response)
  │
  ├── agentkit-tools-core ─ ToolExecutor, PermissionChecker
  │   │                     (step 6, preflight + execute)
  │   │
  │   ├── agentkit-tool-fs ── ReadFileTool, ReplaceInFileTool
  │   │                       (steps 5, 7)
  │   │
  │   └── agentkit-tool-shell ─ ShellExecTool
  │                              (step 8)
  │
  ├── agentkit-task-manager ── AsyncTaskManager, routing
  │                            (step 8, ForegroundThenDetachAfter)
  │
  └── agentkit-reporting ──── StdoutReporter, UsageReporter
                               (every step, event delivery)
```

Every crate has a clear, narrow responsibility. The loop coordinates. Tools execute. Permissions gate. Reporters observe. The host decides.

## Cross-reference

Each step in the walkthrough above maps to a chapter:

| Step                 | Chapter                                                                                                  |
| -------------------- | -------------------------------------------------------------------------------------------------------- |
| 1. Input submission  | [Ch 6: Driving the loop](./ch06-driving-the-loop.md)                                                     |
| 2. Compaction check  | [Ch 16: Transcript compaction](./ch16-compaction.md)                                                     |
| 3. Turn construction | [Ch 5: The model adapter boundary](./ch05-model-adapter.md), [Ch 15: Prompt caching](./ch15-caching.md)  |
| 4. Model invocation  | [Ch 1: Talking to models](./ch01-model-adapter.md), [Ch 4: Streaming](./ch04-streaming-and-deltas.md)    |
| 5. Tool call (read)  | [Ch 11: Filesystem tools](./ch11-filesystem-tools.md)                                                    |
| 6. Permission check  | [Ch 10: Permissions](./ch10-permissions.md)                                                              |
| 7. Tool call (edit)  | [Ch 11: Filesystem tools](./ch11-filesystem-tools.md)                                                    |
| 8. Tool call (shell) | [Ch 12: Shell execution](./ch12-shell-execution.md), [Ch 18: Task management](./ch18-task-management.md) |
| 9. Streaming text    | [Ch 4: Streaming and deltas](./ch04-streaming-and-deltas.md), [Ch 19: Reporting](./ch19-reporting.md)    |
| 10. Turn completion  | [Ch 6: Driving the loop](./ch06-driving-the-loop.md)                                                     |

## Where to go from here

This book has covered the full architecture of an agent system. Some areas for further exploration:

- **Custom providers** — implement adapters for Anthropic, Google, or local model servers using either `CompletionsProvider` (~50 lines) or the raw traits (~200-500 lines)
- **Custom tools** — database queries, API integrations, code analysis, deployment automation
- **[MCP](https://modelcontextprotocol.io) servers** — connect to external tool providers for GitHub, databases, Slack, etc.
- **Advanced compaction** — semantic summarization with a nested agent backend
- **Multi-agent patterns** — tools that spawn sub-agents, parallel agent execution, orchestrator/worker architectures
- **Production hardening** — retry strategies, rate limiting, cost controls, audit logging, persistent sessions

The agentkit crate ecosystem is designed to grow at the edges. The core loop and data model are stable foundations. New tools, providers, and integration patterns can be added without changing the architecture.

| Stable (change rarely)                   | Grows (add freely)                     |
| ---------------------------------------- | -------------------------------------- |
| `agentkit-core` types                    | `agentkit-provider-*` crates           |
| `ModelAdapter` / `ModelSession` traits   | `agentkit-tool-*` crates               |
| `LoopDriver` / `LoopStep`                | `CompactionStrategy` implementations   |
| `Tool` / `ToolSpec` / `ToolRegistry`     | `LoopObserver` implementations         |
| `PermissionChecker` / `PermissionPolicy` | Custom `ContextSource` implementations |
| `Delta` protocol                         | MCP server integrations                |

> **Example:** The [`examples/`](https://github.com/danielkov/agentkit/tree/main/examples) directory in the agentkit repository contains working implementations that exercise every concept in this book, from the simplest chat loop to a full multi-tool coding agent.
