# getting started

## Quick start

1. set `OPENROUTER_API_KEY` in `.env`
2. optionally set `OPENROUTER_MODEL`
3. run one of the examples

Examples:

- `cargo run -p openrouter-chat -- "hello"`
- `cargo run -p openrouter-coding-agent -- "Use fs_read_file on ./Cargo.toml and return only the workspace member count as an integer."`
- `cargo run -p openrouter-agent-cli -- --mcp-mock "Return only the secret from the MCP tool."`

To run the Anthropic example instead, set `ANTHROPIC_API_KEY`,
`ANTHROPIC_MODEL`, and `ANTHROPIC_MAX_TOKENS` (required by the Messages
API), then:

- `cargo run -p anthropic-chat -- --web-search 3 --thinking 2048`

To run the Cerebras examples, set `CEREBRAS_API_KEY` and `CEREBRAS_MODEL`
(both required; `CEREBRAS_BASE_URL`, `CEREBRAS_VERSION_PATCH`, and
`CEREBRAS_MAX_COMPLETION_TOKENS` are optional), then:

- `cargo run -p cerebras-chat -- --reasoning-effort medium`
- `cargo run -p cerebras-batch -- run ./prompts.json`

## Minimal composition

The smallest useful assembly is:

- one model adapter
- one tool registry
- one permission checker
- one loop observer

```rust
let agent = Agent::builder()
    .model(adapter)
    .tools(agentkit_tool_fs::registry())
    .permissions(my_permissions)
    .observer(my_reporter)
    .build()?;
```

Then:

```rust
let mut driver = agent
    .start(SessionConfig {
        session_id: SessionId::new("demo"),
        metadata: MetadataMap::new(),
    })
    .await?;

driver.submit_input(vec![system_item, user_item])?;

match driver.next().await? {
    LoopStep::Finished(result) => { /* render output */ }
    LoopStep::Interrupt(interrupt) => { /* approval, auth, or input */ }
}
```

## Example progression

The examples are meant to build up in complexity:

- `openrouter-chat`
  - provider + loop
- `openrouter-coding-agent`
  - provider + loop + fs tools + permissions
- `openrouter-context-agent`
  - provider + loop + context loading
- `openrouter-mcp-tool`
  - provider + loop + MCP tool adaptation
- `openrouter-subagent-tool`
  - custom tool extension
- `openrouter-acp-trio`
  - three agents exposed as in-memory ACP endpoints, delegating to each other
    over the Agent Client Protocol
- `openrouter-compaction-agent`
  - structural, semantic, and hybrid compaction with a nested-loop compaction backend
- `openrouter-agent-cli`
  - combined example: context + tools + shell + MCP + compaction + reporting
- `anthropic-chat`
  - interactive REPL against Anthropic's Messages API, exercising streaming,
    server tools (web search, web fetch, code execution), extended thinking,
    and the buffered/streaming toggle
- `cerebras-chat`
  - interactive REPL against Cerebras' `/v1/chat/completions`, exposing every
    `CerebrasConfig` knob (sampling, reasoning, response format, compression,
    service tier, predicted outputs, local + MCP tools) as CLI flags, with
    slash commands (`/show`, `/usage`, `/ratelimit`, `/headers`, `/models`,
    `/reset`) for inspecting runtime state
- `cerebras-batch`
  - one-shot CLI over the Files + Batch APIs: `files upload|list|get|content|delete`,
    `batches create|submit|list|get|cancel|wait`, and `run` for the
    submit → wait → fetch happy path
