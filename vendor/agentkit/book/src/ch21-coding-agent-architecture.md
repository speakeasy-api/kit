# Architecture of a coding agent

This chapter steps back from individual crates and looks at how they compose into a complete coding agent — the kind of tool you use when you use Claude Code or Codex CLI.

The previous chapters covered each crate in isolation. This chapter shows how they fit together. The goal is not to document every API — that's what the earlier chapters did. The goal is to show the composition pattern and the trade-offs involved.

## What a coding agent needs

A production coding agent requires all of the pieces we've covered:

| Concern                     | agentkit crate                                                                                                        |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| Transcript and data model   | [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core)                               |
| Capability abstraction      | [`agentkit-capabilities`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-capabilities)               |
| Agent loop and driver       | [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop)                               |
| Tool registry and execution | [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core)                   |
| File read/write/edit        | [`agentkit-tool-fs`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-fs)                         |
| Shell command execution     | [`agentkit-tool-shell`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-shell)                   |
| Project context loading     | [`agentkit-context`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-context)                         |
| Transcript management       | [`agentkit-compaction`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-compaction)                   |
| Async task scheduling       | [`agentkit-task-manager`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-task-manager)               |
| Event reporting             | [`agentkit-reporting`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-reporting)                     |
| LLM provider adapter        | [`agentkit-provider-openrouter`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-openrouter) |

Plus host-specific concerns:

- CLI argument parsing and input handling
- Terminal rendering and streaming output
- Permission policy configuration
- Error recovery and retry strategy
- Session management

## The composition pattern

```rust
// 1. Configure tools
let tools = agentkit_tool_fs::registry()
    .merge(agentkit_tool_shell::registry());

// 2. Configure permissions
let permissions = CompositePermissionChecker::new(PermissionDecision::Deny(default_denial()))
    .with_policy(PathPolicy::new().allow_root(workspace_root))
    .with_policy(CommandPolicy::new().require_approval_for_unknown(true));

// 3. Configure compaction
let compactor = StrategyCompactor::builder()
    .item_count_trigger(20)
    .strategy(
        CompactionPipeline::new()
            .with_strategy(DropReasoningStrategy::new())
            .with_strategy(KeepRecentStrategy::new(12)
                .preserve_kind(ItemKind::System)
                .preserve_kind(ItemKind::Context)),
    )
    .build()?;

// 4. Configure task management
let task_manager = AsyncTaskManager::new().routing(|req: &ToolRequest| {
    if req.tool_name.0 == "shell_exec" {
        RoutingDecision::ForegroundThenDetachAfter(Duration::from_secs(10))
    } else {
        RoutingDecision::Foreground
    }
});

// 5. Configure reporting
let reporter = CompositeReporter::new()
    .with_observer(StdoutReporter::new(std::io::stderr()))
    .with_observer(UsageReporter::new());

// 6. Load context
let context_items = ContextLoader::new()
    .with_source(AgentsMd::discover_all(workspace_root))
    .load()
    .await?;

// 7. Assemble the agent
let agent = Agent::builder()
    .model(OpenRouterAdapter::new(OpenRouterConfig::new(api_key, model))?)
    .add_tool_source(tools)
    .permissions(permissions)
    .compactor(compactor) // AgentBuilderCompactorExt
    .task_manager(task_manager)
    .observer(reporter)
    .build()?;
```

## The host loop

The host application drives the interaction. The system prompt and any preloaded context items are handed to `AgentBuilder::transcript` as the prior transcript; in this turn-based pattern no `input` is preloaded, so every user message flows through the `InputRequest` handle exposed on `AwaitingInput`:

```rust
let mut transcript = Vec::new();
transcript.extend(system_items);
transcript.extend(context_items);

let agent = Agent::builder()
    .model(adapter)
    .transcript(transcript)
    .build()?;

let mut driver = agent.start(session_config).await?;
// With no input preloaded, the first next() yields AwaitingInput — no
// model call is dispatched until the host submits input via the
// InputRequest handle.
let mut pending_input: Option<InputRequest> = None;

loop {
    let req = match pending_input.take() {
        Some(req) => req,
        None => match driver.next().await? {
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => req,
            // Drive any non-AwaitingInput step that surfaces before the
            // first user input (rare; handled defensively).
            other => {
                handle_step(other);
                continue;
            }
        },
    };

    let user_input = read_line()?;
    req.submit(&mut driver, vec![user_item(user_input)])?;

    // Run the agent turn
    loop {
        match driver.next().await? {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                let decision = prompt_user_approval(&pending.request)?;
                driver.resolve_approval(decision)?;
            }
            // Cooperative yield between tool rounds. A headless coding
            // agent just resumes; an interactive host can `info.submit(...)`
            // here to interject a user message before the next model call.
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => {
                pending_input = Some(req);
                break;
            }
            LoopStep::Finished(result) => {
                print_usage(&result);
                break;
            }
        }
    }

    if pending_input.is_none() {
        break;
    }
}
```

MCP-tool auth challenges surface as `ToolError::AuthRequired(_)` rather than a loop interrupt — handle them on the manager via `manager.resolve_auth(...)` (see [Chapter 17](./ch17-mcp.md)).

## Crate dependency graph

```text
agentkit-core                    (no dependencies)
     │
     ├── agentkit-capabilities
     │        │
     │        ├── agentkit-tools-core
     │        │        │
     │        │        ├── agentkit-tool-fs
     │        │        ├── agentkit-tool-shell
     │        │        └── agentkit-tool-skills
     │        │
     │        └── agentkit-mcp
     │
     ├── agentkit-compaction
     │
     ├── agentkit-context
     │
     ├── agentkit-task-manager
     │
     ├── agentkit-reporting
     │
     ├── agentkit-adapter-completions
     │        │
     │        ├── agentkit-provider-openrouter
     │        ├── agentkit-provider-openai
     │        ├── agentkit-provider-ollama
     │        ├── agentkit-provider-vllm
     │        ├── agentkit-provider-groq
     │        └── agentkit-provider-mistral
     │
     └── agentkit-loop          (coordinates everything)
              │
              └── agentkit      (re-exports for convenience)
```

Every crate depends on `agentkit-core`. The loop crate depends on tools, compaction, and task management. Provider crates depend on the completions adapter. Everything else is a leaf.

## Design trade-offs

### Sequential vs parallel tool execution

The default `SimpleTaskManager` is sequential. For a coding agent, this is often fine — file operations are fast and order matters. Shell commands are the exception: builds and tests can take seconds or minutes. `ForegroundThenDetachAfter` gives you the best of both worlds.

| Tool type        | Recommended routing                | Why                   |
| ---------------- | ---------------------------------- | --------------------- |
| Filesystem tools | `Foreground`                       | Fast, order-sensitive |
| Shell tools      | `ForegroundThenDetachAfter(5-10s)` | May be fast or slow   |
| MCP tools        | `Foreground`                       | Usually fast          |

### Compaction strategy

Aggressive compaction loses context. Conservative compaction hits the context window. The right balance depends on the model's context size and the nature of the work.

```text
Recommended starting point:

  Trigger: 20 items
  Pipeline:
    1. DropReasoningStrategy         (reasoning blocks are verbose, rarely needed later)
    2. DropFailedToolResultsStrategy (failed tool results add noise)
    3. KeepRecentStrategy(12)        (keep last 12 non-preserved items)
       .preserve_kind(System)        (system prompt is always needed)
       .preserve_kind(Context)       (project context is always needed)
```

For coding agents, keeping recent tool interactions is usually more valuable than keeping old conversation text — the model needs to know what it just read and edited, not what the user said 20 turns ago.

### Permission posture

Default-deny is safest but requires more approval prompts. Default-allow with denylists is more fluid but riskier. Most coding agents land in the middle:

Recommended permission posture:

| Scope                    | Decision                                          |
| ------------------------ | ------------------------------------------------- |
| Filesystem reads         | `Allow` within workspace                          |
| Filesystem writes        | `Allow` within workspace (with read-before-write) |
| Filesystem outside       | `RequireApproval`                                 |
| Protected files (`.env`) | `Deny`                                            |
| Shell (known safe)       | `Allow` (`git`, `cargo`, `npm`, `ls`, etc.)       |
| Shell (unknown)          | `RequireApproval`                                 |
| Shell (dangerous)        | `Deny` (`rm`, `dd`, `mkfs`)                       |
| MCP (trusted)            | `Allow`                                           |
| MCP (unknown)            | `RequireApproval`                                 |
| Fallback                 | `Deny`                                            |

### What the host owns

agentkit handles the loop, tools, permissions, and streaming. The host application owns everything else:

- **Input/output** — how users type messages and see results
- **Session lifecycle** — when sessions start, end, and resume
- **Error recovery** — what to do when the model fails or rate-limits
- **Configuration** — which model, which tools, which policies
- **Persistence** — saving transcripts, session state, usage logs

The boundary is intentional: agentkit is a library, not a framework. The host is in control.

> **Example:** [`openrouter-agent-cli`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-agent-cli) is the closest existing example to a full coding agent — it combines context, tools, shell, MCP, compaction, and reporting.
