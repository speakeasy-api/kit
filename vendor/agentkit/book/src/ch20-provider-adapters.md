# Provider adapters

Chapter 1 built an adapter from scratch for a hypothetical non-standard API, then introduced the `CompletionsAdapter` for OpenAI-compatible providers. This chapter goes deeper on the `CompletionsProvider` pattern that most real providers use.

## Two paths to an adapter

```text
Path 1: Implement ModelAdapter/ModelSession/ModelTurn directly
  └── For non-standard APIs (custom REST, gRPC, WebSocket)
  └── Full control, full responsibility
  └── ~200-500 lines of translation code

Path 2: Implement CompletionsProvider (via agentkit-adapter-completions)
  └── For OpenAI-compatible chat completions APIs
  └── ~50-100 lines: config + hooks
  └── Transcript conversion, tool serialization, streaming, error handling — all handled
```

Most providers speak the [OpenAI chat completions format](https://platform.openai.com/docs/api-reference/chat) (or close variants). For these, `CompletionsProvider` is the right choice. It handles the ~1000 lines of translation that every completions-compatible adapter needs.

[`agentkit-provider-anthropic`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-anthropic) takes Path 1. Anthropic's `/v1/messages` endpoint has a different shape (top-level `system`, no `tool` role, tool results as content blocks inside user messages, `x-api-key` auth, Anthropic-specific SSE event stream), so it implements `ModelAdapter` directly.

[`agentkit-provider-cerebras`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-cerebras) also takes Path 1, for different reasons. The wire shape is OpenAI-compatible, but the adapter carries surface area that `CompletionsProvider` does not model: msgpack + gzip request compression (behind a Cargo feature), an `X-Cerebras-Version-Patch` header that opts into new API majors, typed reasoning config with model-specific validation, strict JSON-Schema output with the documented constraint checks, a rate-limit snapshot surfaced on the adapter, and a Files + Batch API whose request builder is shared with the turn loop. Folding all of that into generic hooks would dilute them, so the crate talks to `/v1/chat/completions` directly and exposes the preview surfaces through its own types.

## The CompletionsProvider trait

```rust
pub trait CompletionsProvider: Send + Sync + Clone {
    type Config: Serialize + Clone + Send + Sync;

    fn provider_name(&self) -> &str;
    fn endpoint_url(&self) -> &str;
    fn config(&self) -> &Self::Config;

    // Hooks — defaults pass through unchanged:
    fn preprocess_request(&self, builder: HttpRequestBuilder) -> HttpRequestBuilder { builder }
    fn apply_prompt_cache(&self, body: &mut Map<String, Value>, request: &TurnRequest) -> Result<(), LoopError> { Ok(()) }
    fn preprocess_response(&self, _status: StatusCode, _body: &str) -> Result<(), LoopError> { Ok(()) }
    fn postprocess_response(&self, _usage: &mut Option<Usage>, _metadata: &mut MetadataMap, _raw: &Value) {}
}
```

The builder is [`agentkit_http::HttpRequestBuilder`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-http) — a thin transport abstraction. The default `HttpClient` is reqwest-backed; alternative clients (`reqwest-middleware`, or a test double) can be passed via `CompletionsAdapter::with_client`.

The trait has three required methods (name, URL, config) and four optional hooks. Here's what each hook is for:

```text
Request lifecycle with hooks:

  TurnRequest
       │
       ▼
  Build JSON body (transcript → messages, tools → tools array)
  Merge Config fields into body
       │
       ├── preprocess_request(builder) ← add auth headers, custom headers
       │
       ├── apply_prompt_cache(body, request) ← map normalized cache requests
       │
       ▼
  HTTP POST to endpoint_url()
       │
       ▼
  Read response
       │
       ├── preprocess_response(status, body) ← check for API errors in 200 responses
       │
       ▼
  Parse into ModelTurnEvents
       │
       ├── postprocess_response(usage, metadata, raw) ← extract provider-specific fields
       │
       ▼
  Return events to loop
```

## What CompletionsAdapter handles

The generic `CompletionsAdapter<P>` handles all the common work:

| Concern                          | Implementation                                       |
| -------------------------------- | ---------------------------------------------------- |
| `Vec<Item>` → `messages[]`       | Maps all `ItemKind` and `Part` variants              |
| `Vec<ToolSpec>` → `tools[]`      | Converts name, description, JSON Schema              |
| Multimodal content encoding      | Images as `image_url`, audio as `input_audio`        |
| `P::Config` → request body       | Serialize and merge fields                           |
| SSE stream parsing               | Chunk reassembly, delta emission                     |
| Tool call accumulation           | Collect streaming JSON fragments into complete calls |
| `finish_reason` → `FinishReason` | Map provider strings to enum variants                |
| `usage` → `Usage`                | Map token counts and cost                            |
| Cancellation                     | Race HTTP future against `TurnCancellation`          |
| Error status codes               | Convert 4xx/5xx into `LoopError`                     |

## The Config associated type

The `Config` type is where providers differ most. Each provider has different parameter names and supported options:

| Provider | max_tokens field        | Extra fields                            |
| -------- | ----------------------- | --------------------------------------- |
| OpenAI   | `max_completion_tokens` | `frequency_penalty`, `presence_penalty` |
| Ollama   | `num_predict`           | `top_k`                                 |
| Mistral  | `max_tokens`            | —                                       |
| Groq     | `max_completion_tokens` | —                                       |
| vLLM     | `max_tokens`            | —                                       |

By making `Config` an associated type with `Serialize`, each provider declares exactly the fields it supports with their correct names. The adapter serializes the struct and merges it into the request body — no field name mapping needed.

## Building a provider: the pattern

Every provider crate follows the same structure:

```text
agentkit-provider-{name}/
  src/lib.rs
    ├── {Name}Config         // User-facing config (new, with_temperature, from_env, etc.)
    ├── {Name}RequestConfig  // Serializable request fields (#[serde(skip_serializing_if)])
    ├── {Name}Provider       // CompletionsProvider impl
    └── {Name}Adapter        // Newtype over CompletionsAdapter<{Name}Provider>
                             // Implements ModelAdapter by delegation
```

The user-facing API:

```rust
let adapter = OllamaAdapter::new(
    OllamaConfig::new("llama3.1:8b")
        .with_temperature(0.0)
        .with_num_predict(4096),
)?;

let agent = Agent::builder()
    .model(adapter)
    .build()?;
```

## Available providers

agentkit ships eight provider crates. Six go through `CompletionsProvider` (Path 2), and two — Anthropic and Cerebras — implement `ModelAdapter` directly (Path 1):

| Crate                                                                                                                 | Path       | Auth                  | Notes                                                                                                                                          |
| --------------------------------------------------------------------------------------------------------------------- | ---------- | --------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| [`agentkit-provider-openrouter`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-openrouter) | 2 (hooks)  | Bearer + headers      | auth, cache mapping, 200-with-error handling, cost enrichment                                                                                  |
| `agentkit-provider-openai`                                                                                            | 2 (hooks)  | Bearer                | auth, cache mapping                                                                                                                            |
| `agentkit-provider-anthropic`                                                                                         | 1 (direct) | `x-api-key` or Bearer | streaming, extended thinking, server tools, explicit cache-breakpoints, thinking-signature round-trip                                          |
| `agentkit-provider-cerebras`                                                                                          | 1 (direct) | Bearer                | streaming, typed reasoning config, strict JSON-Schema output, rate-limit snapshot, version-patch header; feature-gated compression + Batch API |
| `agentkit-provider-ollama`                                                                                            | 2 (hooks)  | none                  | local runtime; no hooks                                                                                                                        |
| `agentkit-provider-vllm`                                                                                              | 2 (hooks)  | optional Bearer       | `preprocess_request` for optional auth                                                                                                         |
| `agentkit-provider-groq`                                                                                              | 2 (hooks)  | Bearer                | `preprocess_request` for auth                                                                                                                  |
| `agentkit-provider-mistral`                                                                                           | 2 (hooks)  | Bearer                | `preprocess_request` for auth                                                                                                                  |

Ollama is the simplest Path-2 provider — no auth, no hooks. OpenRouter is the most complex Path-2 — auth headers, prompt-cache mapping, 200-with-error handling, response enrichment. Anthropic and Cerebras are the Path-1 providers: read Anthropic if your target API has a non-OpenAI shape, and read Cerebras if it is OpenAI-shaped on the wire but carries enough provider-specific surface — preview parameters, compression, versioning headers, out-of-band endpoints — that modelling it through hooks would be a squeeze.

## When to implement ModelAdapter directly

Use the raw traits when:

- The provider doesn't speak the OpenAI chat completions format
- The provider uses WebSocket or gRPC instead of HTTP
- The provider has server-side session state
- You need streaming behavior that SSE doesn't support

For WebSocket-based providers:

- `start_session` opens the connection
- `begin_turn` sends a continuation frame (not the full transcript)
- `next_event` reads from the live connection
- Session cleanup on drop

## Testing adapters

Whether you use `CompletionsProvider` or implement the raw traits, the normalization contract is the same. Test these guarantees:

1. Text completion → correct `Delta` sequence ending with `CommitPart` and `Finished`
2. Tool calls → `ToolCallPart` with valid IDs and parseable JSON input
3. Multiple tool calls → one `ToolCall` event per call
4. Token limit → `FinishReason::MaxTokens`
5. Cancellation → clean `LoopError::Cancelled`
6. Usage → non-zero, plausible token counts

For `CompletionsProvider` implementations, you mostly need to test the hooks — the generic adapter handles everything else. Mock the HTTP layer with a test server that returns known SSE responses.

> **Crate:** [`agentkit-adapter-completions`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-adapter-completions) — the generic adapter. [`agentkit-provider-*`](https://github.com/danielkov/agentkit/tree/main/crates) — provider-specific implementations.
