# The model adapter boundary

Chapter 1 showed how to build adapters from the outside in — implementing them for specific providers. This chapter looks from the inside out: how the loop consumes the adapter traits, what guarantees it relies on, and what happens when those guarantees are violated.

The adapter boundary is the narrowest point in the architecture. Everything above it (loop logic, tool execution, compaction) is provider-agnostic. Everything below it (HTTP clients, SSE parsing, auth headers) is provider-specific. The three traits define the contract between these two worlds.

## Three-level trait hierarchy

```rust
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    type Session: ModelSession;
    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError>;

    // Optional. Lowercase provider identifier (e.g. `openrouter`,
    // `ollama`) stamped onto the `agent.turn` and `chat` spans as
    // `gen_ai.provider.name`. Defaults to `None`.
    fn provider_name(&self) -> Option<&str> { None }
}

#[async_trait]
pub trait ModelSession: Send {
    type Turn: ModelTurn;
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError>;

    // Optional. Model identifier the session sends requests to, stamped
    // onto the loop's `chat` span as `gen_ai.request.model`. Defaults
    // to `None`.
    fn model_name(&self) -> Option<&str> { None }
}

#[async_trait]
pub trait ModelTurn: Send {
    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError>;
}
```

The `Send + Sync` bound on `ModelAdapter` means adapters can be shared across threads — an `Agent` can be cloned or wrapped in an `Arc` and used from multiple tasks. `ModelSession` is only `Send`, not `Sync` — sessions are single-owner and move between tasks but are not accessed concurrently. `ModelTurn` is likewise `Send` only.

### Why three levels?

The decomposition maps to three distinct lifetimes in a provider interaction:

| Lifetime    | Trait          | State it holds                                                     |
| ----------- | -------------- | ------------------------------------------------------------------ |
| Application | `ModelAdapter` | API key, base URL, HTTP client, model name (immutable, shared)     |
| Session     | `ModelSession` | Conversation ID, WebSocket connection, session token (mutable)     |
| Turn        | `ModelTurn`    | SSE stream, response buffer, chunk parser (mutable, consumed once) |

This supports both stateless and stateful providers:

- **Stateless HTTP providers** (OpenAI, OpenRouter, Groq): `start_session` creates a lightweight handle holding a copy of the config. `begin_turn` sends the full transcript as an HTTP POST. `next_event` reads SSE chunks from the response.

- **Stateful session providers** (WebSocket-based, real-time APIs): `start_session` opens a persistent connection. `begin_turn` sends a delta or continuation message (not the full transcript). `next_event` reads frames from the live connection. Session cleanup happens on drop.

The loop doesn't care which pattern the adapter uses. It calls the same trait methods either way.

```text
Stateless adapter (HTTP):

  Adapter ──start_session──▶ Session (just holds config)
                                │
                                ├──begin_turn──▶ POST /v1/chat/completions
                                │                      │
                                │                Turn ◀┘ (SSE stream handle)
                                │                  │
                                │                  ├── next_event() → Delta
                                │                  ├── next_event() → Delta
                                │                  ├── next_event() → Finished
                                │                  └── next_event() → None
                                │
                                ├──begin_turn──▶ POST /v1/chat/completions
                                │
                                ...


Stateful adapter (WebSocket):

  Adapter ──start_session──▶ Session (owns WebSocket connection)
                                │
                                ├──begin_turn──▶ send continuation frame
                                │                      │
                                │                Turn ◀┘ (reads from same socket)
                                │                  │
                                │                  ├── next_event() → Delta
                                │                  └── next_event() → Finished
                                │
                                ├──begin_turn──▶ send next frame
                                ...
```

## ModelTurnEvent

The model turn emits a stream of normalized events:

```rust
pub enum ModelTurnEvent {
    Delta(Delta),
    ToolCall(ToolCallPart),
    Usage(Usage),
    Finished(ModelTurnResult),
}
```

The adapter is responsible for converting provider-native wire formats into these normalized events. This is where the translation happens — the loop never sees provider-specific response shapes.

The events have a natural ordering within a turn:

```text
Turn event timeline:

  ──────────────────────────────────────────────────────────▶ time

  Delta(BeginPart)
  Delta(AppendText)  ─┐
  Delta(AppendText)   │  streaming text
  Delta(AppendText)  ─┘
  Delta(CommitPart)

  ToolCall(ToolCallPart)    ← fully assembled tool call
  ToolCall(ToolCallPart)    ← another tool call (if parallel)

  Usage(Usage)              ← token counts

  Finished(ModelTurnResult) ← always last
```

`Finished` always comes last. `Usage` typically comes just before `Finished` but some providers interleave it with deltas. `ToolCall` events represent fully assembled tool calls — the adapter has already accumulated the streaming chunks internally.

### ModelTurnResult

```rust
pub struct ModelTurnResult {
    pub finish_reason: FinishReason,
    pub output_items: Vec<Item>,
    pub usage: Option<Usage>,
    pub metadata: MetadataMap,
}
```

The `output_items` field carries the complete assistant response as transcript items. The loop appends these directly to the transcript. `finish_reason` tells the loop what to do next — execute tool calls, return to the host, or handle an error.

## TurnRequest

The loop constructs a `TurnRequest` containing everything the adapter needs:

```rust
pub struct TurnRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub transcript: Vec<Item>,
    pub available_tools: Vec<ToolSpec>,
    pub metadata: MetadataMap,
    pub cache: Option<PromptCacheRequest>,
}
```

The loop owns `TurnRequest` construction. The host doesn't rebuild model-facing state manually each turn. The `transcript` field contains the full conversation so far — system prompt, context items, user messages, assistant responses, tool results. For stateless providers, this is sent in every request. For stateful providers, the adapter decides what subset to send.

`available_tools` contains the tool specifications from the registry. The adapter converts these into the provider's tool schema format (typically `{ "type": "function", "function": { ... } }`).

`metadata` is a pass-through for per-turn options. The host can set provider-specific parameters here without the loop needing to understand them.

`cache` is the normalized prompt caching request for the turn. The adapter maps it into provider-native controls or explicit cache headers. That mapping is covered in [Chapter 15](./ch15-caching.md).

## Cancellation threading

Both `begin_turn` and `next_event` accept an `Option<TurnCancellation>`. The loop creates a checkpoint at the start of each turn and passes it through:

```text
Host calls controller.interrupt()
         │
         ▼
  CancellationController bumps generation (0 → 1)
         │
         ├──▶ TurnCancellation in begin_turn()
         │    checkpoint.is_cancelled() → true
         │    adapter can abort the HTTP request
         │
         └──▶ TurnCancellation in next_event()
              checkpoint.is_cancelled() → true
              adapter can stop reading the SSE stream
```

The adapter should check cancellation at natural yield points — before sending an HTTP request, between SSE chunks, or in a `tokio::select!` race. When cancelled, return `Err(LoopError::Cancelled)` and the loop handles the rest.

## The normalization contract

The adapter has one critical responsibility: produce correct normalized types. The loop's behaviour depends on these guarantees:

| Guarantee                                                  | What happens if violated                                         |
| ---------------------------------------------------------- | ---------------------------------------------------------------- |
| `Finished` is emitted exactly once, as the last event      | Loop hangs or processes stale events                             |
| `FinishReason::ToolCall` when tool calls are present       | Loop ignores tool calls, returns text-only                       |
| `ToolCallPart` has a unique, non-empty `id`                | Tool results can't be correlated; model sees wrong results       |
| `ToolCallPart.input` is valid JSON                         | Tool receives unparseable input, returns an error                |
| `Usage` token counts are accurate                          | Compaction triggers fire at wrong times; cost reporting is wrong |
| `Delta` sequences follow BeginPart → Append\* → CommitPart | Reporter renders garbage; buffer state is inconsistent           |

These are not enforced at the type level — the adapter must get them right. This is the most important surface to test when implementing a new provider.

### Testing the contract

Write tests that verify each guarantee in isolation:

1. Send a simple text prompt → assert `Delta` sequence ends with `CommitPart` and `Finished`
2. Send a prompt that triggers tool calls → assert `ToolCall` events have valid IDs and JSON input
3. Send a prompt that hits the token limit → assert `FinishReason::MaxTokens`
4. Cancel mid-stream → assert the adapter returns `LoopError::Cancelled` cleanly
5. Verify `Usage` token counts are non-zero and plausible

Mock the HTTP layer or use a local test server. Don't test against live provider APIs in CI — they're slow, flaky, and cost money.

## Runtime independence

`agentkit-loop` is runtime-agnostic — it depends on async traits, not on `tokio` directly. The model adapter traits use [`async_trait`](https://docs.rs/async-trait) and require only `Send`, not any runtime-specific bounds.

In practice, most adapters dispatch HTTP through the [`agentkit-http`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-http) crate. The default `HttpClient` is reqwest-backed on tokio; a `reqwest-middleware` client is available behind an optional feature, and custom impls can be supplied for tests. The loop crate itself stays executor-agnostic — `tokio`, `async-std`, or a custom runtime all work — because runtime-specific concerns live in leaf crates (provider adapters, task managers).

The [`futures-timer`](https://docs.rs/futures-timer) crate is used for the cancellation polling delay instead of `tokio::time`, keeping the core free of runtime dependencies.

> **Example:** [`openrouter-chat`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-chat) shows a minimal adapter in action — one model, one session, one turn, rendered to stdout.
>
> **Crate:** The adapter traits are defined in [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop). Provider adapters live in [`agentkit-provider-*`](https://github.com/danielkov/agentkit/tree/main/crates) crates.
