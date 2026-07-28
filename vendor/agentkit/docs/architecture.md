# agentkit architecture

`agentkit` is split into small crates with one umbrella crate for feature-gated re-exports.

## Current crate layout

- `agentkit-core`
  - normalized transcript, part, delta, usage, and tool-call/result types
- `agentkit-compaction`
  - compaction trigger, strategy pipeline, backend hooks, and basic built-ins
- `agentkit-capabilities`
  - lower-level invocable/resource/prompt abstraction
- `agentkit-tools-core`
  - tool traits, registry, executor, permissions, approvals, auth requests
- `agentkit-loop`
  - model session abstraction, driver, interrupts, loop events, tool roundtrips
- `agentkit-context`
  - `AGENTS.md` and skills loading
- `agentkit-mcp`
  - MCP transports, discovery, lifecycle, auth, replay, tool/resource/prompt adapters
- `agentkit-acp`
  - Agent Client Protocol integration: session binding, observer routing, prompt
    conversion, approval resolvers, and a headless stdio runtime built on the
    official `agent-client-protocol` SDK
- `agentkit-reporting`
  - loop observers for stdout, JSONL, usage, transcripts, fanout
- `agentkit-tool-fs`
  - built-in filesystem tools
- `agentkit-tool-shell`
  - built-in shell tool
- `agentkit-tool-skills`
  - progressive Agent Skills discovery and activation
- `agentkit-http`
  - HTTP transport abstraction shared by provider crates (`HttpClient`,
    `Http`, `HttpRequestBuilder`); default reqwest-backed, with optional
    `reqwest-middleware` client
- `agentkit-adapter-completions`
  - generic OpenAI-compatible chat completions adapter base
- `agentkit-provider-openrouter`
  - OpenRouter model adapter
- `agentkit-provider-openai`
  - OpenAI model adapter
- `agentkit-provider-anthropic`
  - Anthropic Messages API adapter; implements `ModelAdapter` directly (the
    Messages API is not OpenAI-compatible)
- `agentkit-provider-cerebras`
  - Cerebras Inference API adapter; implements `ModelAdapter` directly to
    carry provider-specific surface (msgpack/gzip request compression,
    `X-Cerebras-Version-Patch`, typed reasoning config, strict JSON-Schema
    output, rate-limit snapshot, Files + Batch) that does not fit the
    `CompletionsProvider` hook shape
- `agentkit-provider-ollama`
  - Ollama model adapter
- `agentkit-provider-vllm`
  - vLLM model adapter
- `agentkit-provider-groq`
  - Groq model adapter
- `agentkit-provider-mistral`
  - Mistral model adapter
- `agentkit`
  - umbrella crate with feature flags

## Control flow

The main runtime path is:

1. host constructs an `Agent`
2. host starts a `LoopDriver`
3. host submits input items
4. `LoopDriver::next()` runs a model turn
5. non-blocking activity is emitted as `AgentEvent`
6. blocking control points are returned as `LoopStep::Interrupt(...)`
7. when the turn completes, `LoopStep::Finished(...)` is returned

## Blocking vs non-blocking boundaries

Blocking:

- approval requests
- auth requests
- awaiting input

Non-blocking:

- deltas
- usage updates
- tool-call observations
- compaction observations
- warnings

Non-blocking events go to loop observers and reporting adapters. They do not control loop progress.

## Tool execution path

Tool calls requested by the model go through:

1. `ToolRegistry`
2. `ToolExecutor`
3. permission preflight
4. tool invocation
5. normalization into transcript `ToolResult` items

Approval and auth both pause the loop and can resume the original operation.

## MCP path

MCP is split into two layers:

- transport/lifecycle/discovery in `agentkit-mcp`
- adaptation into tools and capabilities for the rest of the stack

MCP auth requests now carry typed operation data and can be resumed through:

- loop auth resume for tool-path interruptions
- `McpServerManager::resolve_auth_and_resume(...)` for non-tool MCP operations

## ACP path

ACP makes agent sessions addressable by external clients (editors, IDEs). The
integration is split the same way as MCP:

- protocol schema, JSON-RPC framing, and transports live in the upstream
  `agent-client-protocol` SDK, re-exported by `agentkit-acp`
- agentkit owns the glue: `AcpIntegration` implements `LoopObserver` and routes
  session-addressed `ObservedEvent`s into per-session ACP `session/update`
  notifications, `AcpInputPort` converts ACP prompts into input items,
  per-session `CancellationController`s service `session/cancel`, and
  `AcpApprovalResolver` bridges loop approval interrupts to ACP
  `session/request_permission`

Hybrid hosts wire those pieces into their own `AgentBuilder`; standalone agents
use `AcpHeadlessRuntime`, which owns the session table and serves ACP over
stdio or a custom transport.

## Mutator path

Transcript edits — compaction, redaction, repair — plug into the loop through one generic seam, `LoopMutator`. Mutators run at well-defined `MutationPoint`s (`AfterToolResult`, `AfterTurnEnded`) and decide for themselves whether to touch the transcript via a `TranscriptCursor`.

For each registered mutator the loop:

1. emits `AgentEvent::MutationStarted { mutator, point, .. }` (mutators are expected to emit their own start/finish events with a stable label)
2. runs the mutator with read-only context and a mutable cursor over the transcript
3. emits `AgentEvent::MutationFinished { mutator, dirty, metadata, .. }`

If any mutator in the pass dirtied the cursor, the loop validates transcript invariants (tool_use ↔ tool_result pairing) and hard-fails with `LoopError::Mutator` if a mutator left the transcript protocol-invalid.

Compaction is the canonical mutator: `agentkit-compaction` provides a `Compactor` trait, `StrategyCompactor`, and trigger helpers (`item_count_trigger`, `context_window_trigger`) that wire into this seam through the `AgentBuilderCompactorExt::compactor` extension. Semantic compaction is provided through an injected `CompactionBackend` (e.g. `AgentCompactor`, which runs a nested sub-agent) rather than a built-in model client.
