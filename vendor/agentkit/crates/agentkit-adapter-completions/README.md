# agentkit-adapter-completions

<p align="center">
  <a href="https://crates.io/crates/agentkit-adapter-completions"><img src="https://img.shields.io/crates/v/agentkit-adapter-completions.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-adapter-completions"><img src="https://img.shields.io/docsrs/agentkit-adapter-completions?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-adapter-completions.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Shared completion-style adapter plumbing for AgentKit model providers.

This crate provides the common request, buffered response, and SSE streaming
translation layer used by providers that expose a completions-compatible API
surface. It is primarily an internal integration crate for provider
implementations such as:

- `agentkit-provider-openai`
- `agentkit-provider-openrouter`
- `agentkit-provider-ollama`
- `agentkit-provider-vllm`
- `agentkit-provider-groq`
- `agentkit-provider-mistral`

Applications will usually depend on a concrete provider crate instead of using
this crate directly.

The adapter normalizes streaming chat-completion chunks into `ModelTurnEvent`s:
text and reasoning arrive as `Delta::AppendText`, tool-call arguments are
accumulated until they parse as JSON, and whole image outputs delivered via
`delta.images[]` are committed as media parts.
