# agentkit

<p align="center">
  <a href="https://crates.io/crates/agentkit"><img src="https://img.shields.io/crates/v/agentkit.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit"><img src="https://img.shields.io/docsrs/agentkit?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Feature-gated umbrella crate for assembling agent applications from the agentkit workspace. Enable only the features you need and access them through a single dependency.

## Default features

`core`, `capabilities`, `tools`, `loop`, `reporting`. The `loop` feature transitively enables `task-manager`.

| Feature        | Module         | Re-exports                                                                                                                |
| -------------- | -------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `core`         | `core`         | `Item`, `Part`, `SessionId`, `TurnId`, `Usage`, `CancellationController`, `TurnCancellation`                              |
| `capabilities` | `capabilities` | `Invocable`, `CapabilityProvider`, `ResourceProvider`, `PromptProvider`                                                   |
| `tools`        | `tools`        | `Tool`, `ToolRegistry`, `ToolSpec`, `BasicToolExecutor`, `PermissionChecker`, `PermissionPolicy`                          |
| `loop`         | `loop_`        | `Agent`, `AgentBuilder`, `LoopDriver`, `LoopStep`, `LoopInterrupt`, `ModelAdapter`, `SessionConfig`, `PromptCacheRequest` |
| `reporting`    | `reporting`    | `StdoutReporter`, `JsonlReporter`, `UsageReporter`, `TranscriptReporter`, `CompositeReporter`                             |

> The agent loop module is re-exported as `loop_` because `loop` is a Rust keyword.

## Optional features

| Feature               | Module                | Purpose                                                                                        |
| --------------------- | --------------------- | ---------------------------------------------------------------------------------------------- |
| `compaction`          | `compaction`          | Transcript compaction triggers, strategies, and pipelines                                      |
| `context`             | `context`             | `AGENTS.md` discovery and context loading                                                      |
| `mcp`                 | `mcp`                 | Model Context Protocol server connections (stdio + Streamable HTTP)                            |
| `acp`                 | `acp`                 | Agent Client Protocol integration: session binding, approval resolvers, headless stdio runtime |
| `task-manager`        | `task_manager`        | Foreground / background tool task scheduling                                                   |
| `adapter-completions` | `adapter_completions` | Generic chat completions adapter base for building provider crates                             |
| `provider-anthropic`  | `provider_anthropic`  | Anthropic Messages API adapter (streaming, prompt caching, extended thinking, server tools)    |
| `provider-cerebras`   | `provider_cerebras`   | Cerebras Inference API adapter (streaming, reasoning, compression, Files + Batch)              |
| `provider-openai`     | `provider_openai`     | OpenAI `/v1/chat/completions` adapter                                                          |
| `provider-openrouter` | `provider_openrouter` | OpenRouter `/v1/chat/completions` adapter                                                      |
| `provider-groq`       | `provider_groq`       | Groq adapter                                                                                   |
| `provider-mistral`    | `provider_mistral`    | Mistral adapter                                                                                |
| `provider-ollama`     | `provider_ollama`     | Ollama adapter                                                                                 |
| `provider-vllm`       | `provider_vllm`       | vLLM adapter                                                                                   |
| `tool-fs`             | `tool_fs`             | Filesystem tools (read, write, edit, move, delete, list, mkdir)                                |
| `tool-shell`          | `tool_shell`          | Shell execution tool (`shell_exec`)                                                            |
| `tool-skills`         | `tool_skills`         | Progressive Agent Skills discovery and activation                                              |

## Quick start

Add agentkit with the features you need:

```toml
[dependencies]
agentkit = { version = "0.10.2", features = ["provider-openrouter", "tool-fs", "tool-shell"] }
tokio = { version = "1", features = ["full"] }
```

## Examples

### Minimal agent with OpenRouter

```rust,no_run
use agentkit::core::{Item, ItemKind};
use agentkit::loop_::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit::provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit::reporting::StdoutReporter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = OpenRouterAdapter::new(OpenRouterConfig::from_env()?)?;

    // Preload the opening user turn through the builder.
    let agent = Agent::builder()
        .model(adapter)
        .observer(StdoutReporter::new(std::io::stdout()))
        .input(vec![Item::text(ItemKind::User, "What is the capital of France?")])
        .build()?;

    let mut driver = agent
        .start(SessionConfig::new("demo").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                println!("Finished: {:?}", result.finish_reason);
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(approval)) => {
                approval.approve(&mut driver)?;
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => {
                // Real apps would prompt the user; here we end the conversation.
                req.submit(&mut driver, vec![])?;
                break;
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
                // Cooperative yield between tool rounds. Resume immediately.
                continue;
            }
        }
    }

    Ok(())
}
```

### Agent with filesystem and shell tools

```rust,no_run
use agentkit::core::{Item, ItemKind};
use agentkit::loop_::{Agent, PromptCacheRequest, PromptCacheRetention, SessionConfig};
use agentkit::provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = OpenRouterAdapter::new(OpenRouterConfig::from_env()?)?;

    let agent = Agent::builder()
        .model(adapter)
        // Each registry is its own ToolSource — register them independently
        // so collisions surface at registration time.
        .add_tool_source(agentkit::tool_fs::registry())
        .add_tool_source(agentkit::tool_shell::registry())
        .input(vec![Item::text(
            ItemKind::User,
            "List the files under ./src and summarise the entry point.",
        )])
        .build()?;

    let _driver = agent.start(
        SessionConfig::new("coding-agent").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ),
    ).await?;

    Ok(())
}
```

### Composing reporters

```rust
use agentkit::reporting::{
    CompositeReporter, JsonlReporter, TranscriptReporter, UsageReporter,
};

let reporter = CompositeReporter::new()
    .with_observer(JsonlReporter::new(Vec::new()))
    .with_observer(UsageReporter::new())
    .with_observer(TranscriptReporter::new());
```

### Bring-your-own `ModelAdapter`

When implementing a custom adapter, only the default features are needed:

```toml
[dependencies]
agentkit = "0.10.2"
```

```rust,ignore
use agentkit::loop_::{Agent, ModelAdapter, SessionConfig};

// Implement ModelAdapter for your backend, then:
// let agent = Agent::builder().model(my_adapter).build()?;
```

See the [book](https://danielkov.github.io/agentkit/) and the in-tree `examples/` directory for end-to-end programs covering streaming, MCP, compaction, parallel tools, session persistence, subagents, and Lua-driven multi-tool composition (`openrouter-codemod`).
