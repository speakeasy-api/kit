# feature flags

The umbrella crate `agentkit` re-exports subcrates behind feature flags.

## Flags

- `core`
  - enables `agentkit-core`
- `compaction`
  - enables `agentkit-compaction`
  - implies `core`
- `capabilities`
  - enables `agentkit-capabilities`
  - implies `core`
- `context`
  - enables `agentkit-context`
  - implies `core`
- `tools`
  - enables `agentkit-tools-core`
  - implies `capabilities`
- `task-manager`
  - enables `agentkit-task-manager`
  - implies `tools`
- `loop`
  - enables `agentkit-loop`
  - implies `tools`, `task-manager`
- `mcp`
  - enables `agentkit-mcp`
  - implies `capabilities`, `tools`
- `acp`
  - enables `agentkit-acp`
  - implies `loop`
- `adapter-completions`
  - enables `agentkit-adapter-completions`
  - implies `loop`
- `provider-anthropic`
  - enables `agentkit-provider-anthropic`
  - implies `loop` (the Messages API is not OpenAI-compatible, so this adapter
    does not go through `adapter-completions`)
- `provider-cerebras`
  - enables `agentkit-provider-cerebras`
  - implies `loop` (carries enough provider-specific surface — compression,
    version-patch header, reasoning config, rate-limit snapshot, Files +
    Batch — that it implements `ModelAdapter` directly instead of routing
    through `adapter-completions`)
- `provider-groq`
  - enables `agentkit-provider-groq`
  - implies `adapter-completions`
- `provider-mistral`
  - enables `agentkit-provider-mistral`
  - implies `adapter-completions`
- `provider-ollama`
  - enables `agentkit-provider-ollama`
  - implies `adapter-completions`
- `provider-openai`
  - enables `agentkit-provider-openai`
  - implies `adapter-completions`
- `provider-openrouter`
  - enables `agentkit-provider-openrouter`
  - implies `adapter-completions`
- `provider-vllm`
  - enables `agentkit-provider-vllm`
  - implies `adapter-completions`
- `reporting`
  - enables `agentkit-reporting`
  - implies `loop`
- `tool-fs`
  - enables `agentkit-tool-fs`
  - implies `tools`
- `tool-shell`
  - enables `agentkit-tool-shell`
  - implies `tools`
- `tool-skills`
  - enables `agentkit-tool-skills`
  - implies `tools`
- `tool-compose`
  - enables `agentkit-tool-compose`
  - implies `tools`

## Default flags

The current default set is:

- `core`
- `capabilities`
- `tools`
- `loop`
- `reporting`

## Typical combinations

Minimal orchestration:

- `core`
- `capabilities`
- `tools`
- `loop`

Coding agent:

- `core`
- `capabilities`
- `context`
- `tools`
- `loop`
- `tool-fs`
- `tool-shell`
- `reporting`

MCP-enabled agent:

- everything above
- `mcp`

ACP-exposed agent (editor-addressable over the Agent Client Protocol):

- everything needed for the host
- `acp`

OpenRouter-backed example host (streaming, prompt caching):

- everything needed for the host
- `provider-openrouter`

OpenAI-compatible provider (streaming; e.g. Groq, Mistral, vLLM, Ollama):

- everything needed for the host
- `provider-groq` / `provider-mistral` / `provider-vllm` / `provider-ollama`

Anthropic Messages API (streaming, extended thinking, server tools):

- everything needed for the host
- `provider-anthropic`

Cerebras Inference API (streaming, reasoning, rate-limit snapshot):

- everything needed for the host
- `provider-cerebras`
- plus, on `agentkit-provider-cerebras` directly, any of:
  - `compression` (msgpack + gzip request bodies)
  - `predicted-outputs`
  - `service-tiers`
  - `batch` (Files + Batch API)
  - `experimental` (umbrella for the three preview flags above)
