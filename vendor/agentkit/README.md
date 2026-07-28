<p align="center">
  <img src="assets/logo.png" alt="agentkit" width="220" />
</p>

<h1 align="center">agentkit</h1>

<p align="center">
  <a href="https://crates.io/crates/agentkit"><img src="https://img.shields.io/crates/v/agentkit.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit"><img src="https://img.shields.io/docsrs/agentkit?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/actions/workflows/book.yml"><img src="https://github.com/danielkov/agentkit/actions/workflows/book.yml/badge.svg" alt="Book" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/crates/l/agentkit.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

`agentkit` is a Rust toolkit for building LLM agent applications such as coding agents, assistant CLIs, and multi-agent tools.

The project is split into small crates behind feature flags so hosts can pull in only the pieces they need.

## Usage

```rust,ignore
use agentkit_core::{Item, ItemKind};
use agentkit_loop::{
    Agent, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = OpenRouterConfig::from_env()?;
    let adapter = OpenRouterAdapter::new(config)?;

    let agent = Agent::builder()
        .model(adapter)
        .input(vec![Item::text(ItemKind::User, "Hello!")])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("chat").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    if let LoopStep::Finished(result) = driver.next().await? {
        println!("Finished: {:?}", result.finish_reason);
    }
    Ok(())
}
```

## Crates

- `agentkit-core`
  - transcript, parts, deltas, IDs, usage, and cancellation primitives
- `agentkit-capabilities`
  - lower-level invocable/resource/prompt abstraction
- `agentkit-tools-core`
  - tools, registry, executor, permissions, approvals
- `agentkit-loop`
  - model session abstraction, driver, interrupts, tool roundtrips
- `agentkit-context`
  - `AGENTS.md` and skills loading
- `agentkit-mcp`
  - MCP integration built on `rmcp`: stdio + Streamable HTTP transports, discovery, lifecycle, auth + replay, tool/resource/prompt adapters, sampling/elicitation/roots responders, and a server-event broadcast
- `agentkit-acp`
  - Agent Client Protocol integration built on the official `agent-client-protocol` SDK: session binding, observer routing, prompt conversion, approval resolvers, and a headless stdio runtime for standalone ACP agents
- `agentkit-reporting`
  - loop observers and reporting adapters
- `agentkit-compaction`
  - compaction triggers, strategies, pipelines, backend hooks
- `agentkit-task-manager`
  - task scheduling for tool execution: foreground, background, and detach-after-timeout routing
- `agentkit-tool-fs`
  - filesystem tools
- `agentkit-tool-shell`
  - shell execution tool
- `agentkit-tool-skills`
  - progressive skill discovery and activation
- `agentkit-http`
  - HTTP transport abstraction (`HttpClient`, `Http`, `HttpRequestBuilder`) with a default reqwest-backed implementation and an optional `reqwest-middleware` adapter
- `agentkit-adapter-completions`
  - generic chat completions adapter base with buffered and SSE streaming turns
- `agentkit-provider-openrouter`
  - OpenRouter adapter with streaming, tool calls, multimodal content, and prompt caching
- `agentkit-provider-openai`
  - OpenAI adapter with streaming, tool calls, multimodal content, and prompt caching
- `agentkit-provider-anthropic`
  - Anthropic Messages API adapter with streaming, prompt caching, extended thinking, and server-side tools (web search, web fetch, code execution)
- `agentkit-provider-cerebras`
  - Cerebras Inference API adapter with streaming, reasoning, strict JSON schema, compression (msgpack/gzip), predicted outputs, service tiers, and Files + Batch API
- `agentkit-provider-ollama`
  - Ollama adapter with streaming
- `agentkit-provider-vllm`
  - vLLM adapter with streaming
- `agentkit-provider-groq`
  - Groq adapter with streaming
- `agentkit-provider-mistral`
  - Mistral adapter with streaming
- `agentkit`
  - umbrella crate with feature-gated re-exports

## Built-in tools today

Filesystem:

- `fs_read_file`
  - supports optional `from` / `to` line ranges
- `fs_write_file`
- `fs_replace_in_file`
- `fs_move`
- `fs_delete`
- `fs_list_directory`
- `fs_create_directory`

Shell:

- `shell_exec`

The filesystem crate also supports session-scoped read-before-write enforcement through `FileSystemToolResources` and `FileSystemToolPolicy`.

## Quick start

1. Set your OpenRouter API key and model — either through environment variables or directly in code via `OpenRouterConfig::new(api_key, model)`.
2. Run one of the examples.

Example commands:

```bash
cargo run -p openrouter-chat -- "hello"
```

```bash
cargo run -p openrouter-coding-agent -- \
  "Use fs_read_file on ./Cargo.toml and return only the workspace member count as an integer."
```

```bash
cargo run -p openrouter-agent-cli -- --mcp-mock \
  "Return only the secret from the MCP tool."
```

## Example progression

- `openrouter-chat`
  - minimal chat loop
  - now supports `Ctrl-C` turn cancellation
- `openrouter-coding-agent`
  - interactive coding-agent host with streaming delta rendering and filesystem tools
- `openrouter-context-agent`
  - context loading from `AGENTS.md` and skills
- `openrouter-mcp-tool`
  - MCP tool discovery and invocation
- `openrouter-subagent-tool`
  - custom tool that runs a nested agent
- `openrouter-acp-trio`
  - three agents (orchestrator, worker, reviewer) exposed as in-memory ACP endpoints, delegating to each other over the Agent Client Protocol
- `openrouter-compaction-agent`
  - structural, semantic, and hybrid compaction
  - semantic compaction uses a nested agent as the backend
- `openrouter-parallel-agent`
  - async task manager with foreground fs tools and detach-after-timeout shell tools
  - `TaskManagerHandle` event stream printed to stderr
- `openrouter-agent-cli`
  - combined example using context, tools, shell, MCP, compaction, and reporting
- `anthropic-chat`
  - streaming REPL against Anthropic's Messages API, with server tools
    (`--web-search`, `--web-fetch`, `--code-exec`), extended thinking
    (`--thinking`), and a streaming / buffered toggle (`--streaming` /
    `--no-streaming`)
- `cerebras-chat`
  - interactive REPL against Cerebras `/v1/chat/completions`; CLI flags
    cover every `CerebrasConfig` knob (sampling, reasoning, response
    format, compression, service tier, predicted outputs, local tools)
    and slash commands (`/show`, `/usage`, `/ratelimit`, `/headers`,
    `/models`, `/reset`) surface runtime state
- `cerebras-batch`
  - one-shot CLI over the Cerebras Files + Batch APIs: `files upload|list|get|content|delete`,
    `batches create|submit|list|get|cancel|wait`, and `run` to submit → wait → dump outputs

## Examples

### Minimal chat

Build an agent with a provider adapter and an opening user turn, then drive the loop:

```rust
use agentkit_core::{Item, ItemKind};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

let adapter = OpenRouterAdapter::new(
    OpenRouterConfig::new("sk-or-v1-...", "openrouter/auto")
        .with_temperature(0.0),
)?;

let agent = Agent::builder()
    .model(adapter)
    // Optional — preload a prior transcript (system prompt or resumed
    // session) and the next user turn. Both default to empty.
    .input(vec![Item::text(ItemKind::User, "Hello!")])
    .build()?;

let mut driver = agent
    .start(SessionConfig::new("chat").with_cache(
        PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
    ))
    .await?;

// First next() dispatches the model directly because we preloaded input.
match driver.next().await? {
    LoopStep::Finished(result) => { /* render result.items */ }
    LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
        /* blocking: approve or deny via the PendingApproval handle */
    }
    LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => {
        /* cooperative: req.submit(&mut driver, more_items)? then call next() */
    }
    LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => { /* call next() to resume */ }
}
```

`AgentBuilder::transcript` preloads the prior transcript as passive starting state — typically `[system_item]` for a fresh session, or a transcript loaded from disk when resuming. `AgentBuilder::input` preloads the next user turn into the driver's pending-input queue: when non-empty, the first `next()` dispatches the model directly; when left empty (the default for turn-based loops), the first `next()` yields `AwaitingInput` and every user turn flows through the `InputRequest` / `ToolRoundInfo` handles surfaced on the cooperative interrupts. There is no out-of-turn `submit_input` entry point.

### Tools and permissions

Register filesystem tools with a path-scoped permission policy. Tool sources federate — call `add_tool_source` once per source (registry, MCP catalog reader, skill watcher, …) and the agent walks them in registration order:

```rust
use agentkit_core::MetadataMap;
use agentkit_loop::Agent;
use agentkit_tools_core::{
    CompositePermissionChecker, PathPolicy, PermissionCode, PermissionDecision, PermissionDenial,
};

let permissions = CompositePermissionChecker::new(PermissionDecision::Deny(PermissionDenial {
    code: PermissionCode::UnknownRequest,
    message: "not allowed by policy".into(),
    metadata: MetadataMap::new(),
}))
.with_policy(
    PathPolicy::new()
        .allow_root(std::env::current_dir()?)
        .require_approval_outside_allowed(false),
);

let agent = Agent::builder()
    .model(adapter)
    .add_tool_source(agentkit_tool_fs::registry())
    .permissions(permissions)
    .build()?;
```

### Reporting

Compose multiple observers to log output, track usage, and record transcripts:

```rust
use agentkit_reporting::{CompositeReporter, JsonlReporter, StdoutReporter, UsageReporter};

let reporter = CompositeReporter::new()
    .with_observer(StdoutReporter::new(std::io::stderr()).with_usage(false))
    .with_observer(JsonlReporter::new(Vec::new()))
    .with_observer(UsageReporter::new());

let agent = Agent::builder()
    .model(adapter)
    .observer(reporter)
    .build()?;
```

### Compaction

Configure structural compaction that drops reasoning and failed tool results, then keeps the most recent items:

```rust
use agentkit_compaction::{
    AgentBuilderCompactorExt, CompactionPipeline, DropFailedToolResultsStrategy,
    DropReasoningStrategy, KeepRecentStrategy, StrategyCompactor,
};
use agentkit_core::ItemKind;

let compactor = StrategyCompactor::builder()
    .item_count_trigger(10)
    .strategy(
        CompactionPipeline::new()
            .with_strategy(DropReasoningStrategy::new())
            .with_strategy(DropFailedToolResultsStrategy::new())
            .with_strategy(
                KeepRecentStrategy::new(8)
                    .preserve_kind(ItemKind::System)
                    .preserve_kind(ItemKind::Context),
            ),
    )
    .build()?;

let agent = Agent::builder()
    .model(adapter)
    .compactor(compactor)
    .build()?;
```

Compactors plug into the loop's generic `LoopMutator` seam, so the same hook handles redaction, repair, or any other transcript edit. Use `context_window_trigger(window, percent)` for token-aware triggering driven by provider-reported `input_tokens`.

### Async task management

Route shell commands to background execution with automatic detach-after-timeout:

```rust
use agentkit_task_manager::{AsyncTaskManager, RoutingDecision};
use std::time::Duration;

let task_manager = AsyncTaskManager::new().routing(|req: &agentkit_tools_core::ToolRequest| {
    if req.tool_name.0 == "shell_exec" {
        RoutingDecision::ForegroundThenDetachAfter(Duration::from_secs(5))
    } else {
        RoutingDecision::Foreground
    }
});

let agent = Agent::builder()
    .model(adapter)
    .add_tool_source(tools)
    .task_manager(task_manager)
    .build()?;
```

## Feature flags

The umbrella crate re-exports subcrates behind feature flags.

Default flags:

- `core`
- `capabilities`
- `tools`
- `task-manager`
- `loop`
- `reporting`

Optional flags:

- `acp`
- `compaction`
- `context`
- `mcp`
- `adapter-completions`
- `provider-openrouter`
- `provider-openai`
- `provider-anthropic`
- `provider-cerebras`
- `provider-ollama`
- `provider-vllm`
- `provider-groq`
- `provider-mistral`
- `tool-fs`
- `tool-shell`
- `tool-skills`

More detail is in [docs/feature-flags.md](./docs/feature-flags.md).

## Docs

- [docs/getting-started.md](./docs/getting-started.md)
- [docs/architecture.md](./docs/architecture.md)
- [docs/core.md](./docs/core.md)
- [docs/tools.md](./docs/tools.md)
- [docs/loop.md](./docs/loop.md)
- [docs/permissions.md](./docs/permissions.md)
- [docs/capabilities.md](./docs/capabilities.md)
- [docs/context.md](./docs/context.md)
- [docs/mcp.md](./docs/mcp.md)
- [docs/acp.md](./docs/acp.md)
- [docs/compaction.md](./docs/compaction.md)
- [docs/reporting.md](./docs/reporting.md)
- [docs/feature-flags.md](./docs/feature-flags.md)
- [docs/README.md](./docs/README.md)
