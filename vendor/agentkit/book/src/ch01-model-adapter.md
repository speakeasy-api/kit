# Talking to models

Chapter 0 covered how LLMs work internally — tokenisation, attention, sampling. This chapter covers the practical question: how does your code send a transcript to a model and get a response back?

The answer depends on where the model runs and how the provider exposes it. agentkit abstracts over these differences with three traits: `ModelAdapter`, `ModelSession`, and `ModelTurn`. This chapter introduces the traits, builds an adapter from scratch for a hypothetical non-standard API, then shows how `agentkit-adapter-completions` handles the common case for OpenAI-compatible providers.

## Transport: local vs remote

Model providers fall into two categories:

|               | Local                            | Remote                        |
| ------------- | -------------------------------- | ----------------------------- |
| Where it runs | On your machine                  | On provider infra             |
| Transport     | HTTP to localhost                | HTTP to provider API          |
| Auth          | None required                    | API key / OAuth               |
| Resource mgmt | You manage GPU/CPU               | Provider manages scaling      |
| Examples      | Ollama, llama.cpp, vLLM, LocalAI | OpenRouter, Anthropic, OpenAI |

Both categories use HTTP and the [OpenAI-compatible chat completions format](https://platform.openai.com/docs/api-reference/chat) (or close variants of it). The differences are in authentication, endpoint URLs, and which features are supported (streaming, tool calling, multimodal inputs).

From an adapter's perspective, the transport is the same — an HTTP POST with a JSON body. What varies is:

- **authentication**: local servers typically need none; remote providers require API keys or headers
- **request schema**: most providers follow the OpenAI chat completions shape, with provider-specific extensions
- **response shape**: the same choices and message structure, with varying support for tool calls, usage reporting, and reasoning output
- **streaming**: some providers return a single JSON response; others stream [server-sent events (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events/Using_server-sent_events)

## The chat completions format

Most providers (including Ollama, OpenRouter, OpenAI, and many others) speak the same wire format: the OpenAI chat completions API. Understanding this format is essential for adapter work, because the adapter's job is to translate between it and agentkit's transcript model.

### Request

A chat completion request is a JSON POST body with three key fields:

```json
{
  "model": "llama3.1:8b",
  "messages": [
    { "role": "system", "content": "You are a helpful assistant." },
    { "role": "user", "content": "What is 2 + 2?" }
  ],
  "stream": false
}
```

The `messages` array is the transcript. Each message has a `role` and `content`. The roles map to agentkit's `ItemKind`:

| Chat completions role | agentkit `ItemKind`              |
| --------------------- | -------------------------------- |
| `system`              | `System`, `Developer`, `Context` |
| `user`                | `User`                           |
| `assistant`           | `Assistant`                      |
| `tool`                | `Tool`                           |

When tools are available, the request includes a `tools` array describing each tool's name, description, and JSON Schema for its parameters:

```json
{
  "model": "llama3.1:8b",
  "messages": [ ... ],
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "read_file",
        "description": "Read a file from disk",
        "parameters": {
          "type": "object",
          "properties": {
            "path": { "type": "string" }
          },
          "required": ["path"]
        }
      }
    }
  ]
}
```

Optional fields include `temperature`, `max_completion_tokens`, `top_p`, and provider-specific extensions.

### Response (non-streaming)

The response wraps the model's output in a `choices` array:

```json
{
  "id": "chatcmpl-abc123",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "2 + 2 = 4."
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 25,
    "completion_tokens": 8,
    "total_tokens": 33
  }
}
```

`finish_reason` tells you why the model stopped:

| `finish_reason`  | Meaning                   | agentkit `FinishReason` |
| ---------------- | ------------------------- | ----------------------- |
| `stop`           | Model finished normally   | `Completed`             |
| `tool_calls`     | Model wants to call tools | `ToolCall`              |
| `length`         | Hit token limit           | `MaxTokens`             |
| `content_filter` | Blocked by safety filter  | `Blocked`               |

When the model calls tools, the `message` includes a `tool_calls` array instead of (or alongside) `content`:

```json
{
  "choices": [
    {
      "message": {
        "role": "assistant",
        "content": null,
        "tool_calls": [
          {
            "id": "call_abc123",
            "type": "function",
            "function": {
              "name": "read_file",
              "arguments": "{\"path\": \"src/main.rs\"}"
            }
          }
        ]
      },
      "finish_reason": "tool_calls"
    }
  ]
}
```

Note that `arguments` is a JSON string, not a JSON object — it needs an extra parse step.

To send tool results back, you append messages with `role: "tool"`:

```json
{
  "role": "tool",
  "tool_call_id": "call_abc123",
  "content": "fn main() { ... }"
}
```

### Streaming (SSE)

When `"stream": true`, the response is a series of [server-sent events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events/Using_server-sent_events). Each event carries a `delta` (partial content) instead of a complete `message`:

```text
data: {"choices":[{"delta":{"role":"assistant","content":"2"},"index":0}]}

data: {"choices":[{"delta":{"content":" +"},"index":0}]}

data: {"choices":[{"delta":{"content":" 2"},"index":0}]}

data: {"choices":[{"delta":{"content":" = 4."},"index":0,"finish_reason":"stop"}]}

data: [DONE]
```

The consumer reassembles the full message by concatenating `delta.content` chunks. Tool call arguments also stream incrementally. This is where agentkit's `Delta` type comes in — it provides a structured representation of these incremental updates. Streaming is covered in detail in a later chapter.

### What an adapter does with this

An adapter's job is two translations:

```text
agentkit → provider (request):
  Vec<Item>                         ──▶ messages[]
  Vec<ToolSpec>                     ──▶ tools[]
  SessionConfig / TurnRequest.cache ──▶ auth headers, model field, cache controls

provider → agentkit (response):
  choices[0].message            ──▶ Item { kind: Assistant, parts: [...] }
  choices[0].message.tool_calls ──▶ ToolCallPart per call
  usage                         ──▶ Usage { tokens: TokenUsage { ... } }
  finish_reason                 ──▶ FinishReason
```

The rest of this chapter shows how these translations map to agentkit's adapter traits.

## The adapter traits

agentkit defines three traits that model the lifecycle of talking to a provider:

```text
ModelAdapter          ModelSession      ModelTurn
────────────          ────────────      ─────────
start_session() ──▶   begin_turn() ──▶  next_event() ──▶ ModelTurnEvent
                      begin_turn() ──▶  next_event() ──▶ ModelTurnEvent
                      begin_turn() ──▶  ...
                                        next_event() ──▶ None (exhausted)
```

- **`ModelAdapter`** — a factory. It holds configuration (API keys, model name, HTTP client) and produces sessions. It is `Send + Sync` so it can be shared across threads.
- **`ModelSession`** — a connection-scoped handle. Created once per agent session, it may hold state that persists across turns (e.g. a conversation ID for stateful APIs). Each call to `begin_turn()` sends the full transcript to the provider and returns a turn.
- **`ModelTurn`** — a streaming response handle. The loop calls `next_event()` repeatedly until it returns `None` or a `Finished` event. For non-streaming providers, all events can be buffered upfront and drained from a queue.

The trait signatures:

```rust
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    type Session: ModelSession;
    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError>;
}

#[async_trait]
pub trait ModelSession: Send {
    type Turn: ModelTurn;
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError>;
}

#[async_trait]
pub trait ModelTurn: Send {
    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError>;
}
```

`TurnRequest` carries everything the provider needs:

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

`ModelTurnEvent` is what comes back:

```rust
pub enum ModelTurnEvent {
    Delta(Delta),
    ToolCall(ToolCallPart),
    Usage(Usage),
    Finished(ModelTurnResult),
}
```

## Building an adapter from scratch

To see what the traits require, consider a hypothetical model provider that does not use the OpenAI format. Suppose "AcmeAI" has a proprietary REST API:

```text
POST https://api.acme.ai/v1/generate
Authorization: Bearer <token>

{
  "prompt": "What is 2 + 2?",
  "system_instruction": "You are a helpful assistant.",
  "config": { "temperature": 0.5, "max_tokens": 256 }
}

Response:
{
  "text": "2 + 2 = 4.",
  "tokens_used": { "input": 25, "output": 8 },
  "stop_reason": "complete"
}
```

No `messages` array. No `choices` wrapper. No `tool_calls`. A completely different shape. The adapter must translate to and from it.

### Adapter and session

```rust
pub struct AcmeAdapter {
    client: Client,
    api_key: String,
}

pub struct AcmeSession {
    client: Client,
    api_key: String,
}

#[async_trait]
impl ModelAdapter for AcmeAdapter {
    type Session = AcmeSession;

    async fn start_session(&self, _config: SessionConfig) -> Result<AcmeSession, LoopError> {
        Ok(AcmeSession {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
        })
    }
}
```

### Turn: the translation layer

`begin_turn` does the work. It must convert agentkit's transcript into Acme's request format and convert the response back into `ModelTurnEvent`s:

```rust
#[async_trait]
impl ModelSession for AcmeSession {
    type Turn = AcmeTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Result<AcmeTurn, LoopError> {
        // Extract the last user message as the prompt.
        // Acme doesn't support multi-turn — flatten the transcript.
        let prompt = request.transcript.iter()
            .rev()
            .find(|item| item.kind == ItemKind::User)
            .and_then(|item| item.parts.first())
            .and_then(|part| match part {
                Part::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default();

        let system = request.transcript.iter()
            .find(|item| item.kind == ItemKind::System)
            .and_then(|item| item.parts.first())
            .and_then(|part| match part {
                Part::Text(t) => Some(t.text.clone()),
                _ => None,
            });

        let body = json!({
            "prompt": prompt,
            "system_instruction": system,
            "config": { "temperature": 0.5, "max_tokens": 256 },
        });

        let resp: AcmeResponse = self.client
            .post("https://api.acme.ai/v1/generate")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send().await
            .map_err(|e| LoopError::Provider(e.to_string()))?
            .json().await
            .map_err(|e| LoopError::Provider(e.to_string()))?;

        // Convert Acme's response into ModelTurnEvents
        let mut events = VecDeque::new();

        events.push_back(ModelTurnEvent::Usage(Usage {
            tokens: Some(TokenUsage {
                input_tokens: resp.tokens_used.input,
                output_tokens: resp.tokens_used.output,
                reasoning_tokens: None,
                cached_input_tokens: None,
            }),
            cost: None,
            metadata: MetadataMap::new(),
        }));

        let output_item = Item {
            id: None,
            kind: ItemKind::Assistant,
            parts: vec![Part::Text(TextPart {
                text: resp.text,
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
        };

        let finish_reason = match resp.stop_reason.as_str() {
            "complete" => FinishReason::Completed,
            "max_tokens" => FinishReason::MaxTokens,
            other => FinishReason::Other(other.into()),
        };

        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason,
            output_items: vec![output_item],
            usage: None,
            metadata: MetadataMap::new(),
            // Optional response identity for the loop's `chat` telemetry
            // span (`gen_ai.response.model` / `gen_ai.response.id`).
            model: None,
            response_id: None,
        }));

        Ok(AcmeTurn { events })
    }
}
```

### The turn drain

The turn itself is the same `VecDeque` pattern used by every non-streaming adapter:

```rust
pub struct AcmeTurn {
    events: VecDeque<ModelTurnEvent>,
}

#[async_trait]
impl ModelTurn for AcmeTurn {
    async fn next_event(
        &mut self,
        _cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.events.pop_front())
    }
}
```

This adapter is complete. It can be passed to `Agent::builder().model(adapter)` and the loop will call it. The model doesn't support tool calls, so the loop will always finish after a single turn — but that's a limitation of Acme's API, not of the adapter.

The key takeaway: you can integrate any model provider by implementing the three traits. The translation is manual — you map the provider's request/response format to agentkit's `Item`/`Part`/`Usage`/`FinishReason` types. There is no requirement that the provider speaks OpenAI's format.

## The completions adapter

Most providers _do_ speak the OpenAI chat completions format. Implementing the full translation for each one — transcript conversion, multimodal content encoding, tool call parsing, cancellation, error handling — is repetitive. The [`agentkit-adapter-completions`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-adapter-completions) crate handles all of it once.

Instead of implementing `ModelAdapter` / `ModelSession` / `ModelTurn` directly, a provider implements `CompletionsProvider`:

```rust
pub trait CompletionsProvider: Send + Sync + Clone {
    /// Strongly-typed request config (model, temperature, etc.).
    /// Serialised and merged into the request body.
    type Config: Serialize + Clone + Send + Sync;

    fn provider_name(&self) -> &str;
    fn endpoint_url(&self) -> &str;
    fn config(&self) -> &Self::Config;

    // Hooks — defaults pass through unchanged:
    fn preprocess_request(&self, builder: agentkit_http::HttpRequestBuilder) -> agentkit_http::HttpRequestBuilder { builder }
    fn preprocess_response(&self, _status: StatusCode, _body: &str) -> Result<(), LoopError> { Ok(()) }
    fn postprocess_response(&self, _usage: &mut Option<Usage>, _metadata: &mut MetadataMap, _raw: &Value) {}
}
```

> The request builder is [`agentkit_http::HttpRequestBuilder`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-http) — a thin transport abstraction over `reqwest`. The default `HttpClient` is reqwest-backed; the `reqwest-middleware` client is available behind an optional feature, and custom impls can be supplied for tests.

The generic `CompletionsAdapter<P>` implements `ModelAdapter` and handles:

- Converting `Vec<Item>` to the `messages` array (all `ItemKind` and `Part` variants)
- Serialising `P::Config` and merging it into the request body
- Converting `Vec<ToolSpec>` to the `tools` array
- Parsing the response into `ModelTurnEvent`s (text, tool calls, reasoning, usage, finish reason)
- Encoding multimodal content (images as `image_url`, audio as `input_audio`)
- Racing the HTTP future against the cancellation handle

```text
┌────────────────────────────────────────────────────────────┐
│  CompletionsAdapter<P>                                     │
│                                                            │
│  ┌────────────────────────┐  ┌──────────────────────────┐  │
│  │ P: CompletionsProvider │  │ request.rs / response.rs │  │
│  │ (endpoint, config,     │  │ (transcript conversion,  │  │
│  │  pre/post hooks)       │  │  response parsing)       │  │
│  └────────────────────────┘  └──────────────────────────┘  │
│                                                            │
│  Implements ModelAdapter ──▶ ModelSession ──▶ ModelTurn    │
└────────────────────────────────────────────────────────────┘
```

The `Config` associated type is generic because request parameters differ across providers — and sometimes across models within the same provider. Ollama uses `num_predict` where OpenAI uses `max_completion_tokens`. Mistral uses `max_tokens`. Some providers support `top_k`, others don't. Making this a provider-defined `Serialize` struct means each provider declares exactly the parameters it supports, with their correct field names, and gets compile-time validation and IDE completion. The adapter serialises the struct and merges it into the request body:

```rust
// In the adapter's request builder:
let config_value = serde_json::to_value(provider.config())?;
if let Value::Object(fields) = config_value {
    for (key, value) in fields {
        body.insert(key, value);
    }
}
```

This means Ollama can use `num_predict` where OpenAI uses `max_completion_tokens`, Mistral can use `max_tokens`, and each provider gets IDE completion and compile-time validation for its supported parameters.

## Building an Ollama provider

Ollama exposes an OpenAI-compatible endpoint at `http://localhost:11434/v1/chat/completions`. Using `agentkit-adapter-completions`, the entire provider is a config struct, a request config struct, and a `CompletionsProvider` impl.

### Configuration

The user-facing config holds connection and inference parameters:

```rust
pub struct OllamaConfig {
    pub model: String,
    pub base_url: String,
    pub temperature: Option<f32>,
    pub num_predict: Option<u32>,
    pub top_k: Option<u32>,
    pub top_p: Option<f32>,
}

impl OllamaConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: "http://localhost:11434/v1/chat/completions".into(),
            temperature: None,
            num_predict: None,
            top_k: None,
            top_p: None,
        }
    }

    pub fn with_temperature(mut self, v: f32) -> Self {
        self.temperature = Some(v);
        self
    }

    pub fn with_num_predict(mut self, v: u32) -> Self {
        self.num_predict = Some(v);
        self
    }
    // ...
}
```

### Request config

The request config is what gets serialised into the JSON body. It uses `#[serde(skip_serializing_if)]` so unset parameters are omitted, not sent as `null`:

```rust
#[derive(Clone, Serialize)]
pub struct OllamaRequestConfig {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
}
```

### Provider implementation

The provider struct holds connection details and the request config. The `CompletionsProvider` impl is minimal — Ollama has no auth and no protocol quirks:

```rust
#[derive(Clone)]
pub struct OllamaProvider {
    base_url: String,
    request_config: OllamaRequestConfig,
}

impl CompletionsProvider for OllamaProvider {
    type Config = OllamaRequestConfig;

    fn provider_name(&self) -> &str { "Ollama" }
    fn endpoint_url(&self) -> &str { &self.base_url }
    fn config(&self) -> &OllamaRequestConfig { &self.request_config }
}
```

No hooks overridden. Ollama needs no auth, has no response quirks, and reports no provider-specific fields. The defaults pass everything through unchanged.

### The adapter newtype

The adapter is a newtype over `CompletionsAdapter<OllamaProvider>`, delegating to it for the `ModelAdapter` impl:

```rust
#[derive(Clone)]
pub struct OllamaAdapter(CompletionsAdapter<OllamaProvider>);

impl OllamaAdapter {
    pub fn new(config: OllamaConfig) -> Result<Self, OllamaError> {
        let provider = OllamaProvider::from(config);
        Ok(Self(CompletionsAdapter::new(provider)?))
    }
}

#[async_trait]
impl ModelAdapter for OllamaAdapter {
    type Session = CompletionsSession<OllamaProvider>;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        self.0.start_session(config).await
    }
}
```

This is the complete provider. All of the transcript conversion, tool call serialisation, response parsing, multimodal encoding, and cancellation handling comes from `agentkit-adapter-completions`.

### Contrast: what the adapter handles vs what the provider handles

| `agentkit-adapter-completions` | `agentkit-provider-ollama`               |
| ------------------------------ | ---------------------------------------- |
| `Vec<Item>` → `messages[]`     | endpoint URL                             |
| `Vec<ToolSpec>` → `tools[]`    | request config (model, temperature, ...) |
| `Config` → request body fields | `preprocess_request` (none needed)       |
| response → `ModelTurnEvent`    | `preprocess_response` (none needed)      |
| multimodal content encoding    | `postprocess_response` (none needed)     |
| cancellation                   |                                          |
| error status codes             |                                          |
| tool call parsing              |                                          |
| usage mapping                  |                                          |
| finish reason mapping          |                                          |

## Providers with quirks

Not all OpenAI-compatible providers are identical. The three hooks exist for providers that need to customise the standard request/response flow.

OpenRouter uses all three:

1. **`preprocess_request`** — adds bearer auth, `X-Title`, and `HTTP-Referer` headers
2. **`preprocess_response`** — the API sometimes returns HTTP 200 with an error payload instead of a proper error status; the hook parses these and converts them to errors before the adapter attempts normal deserialization
3. **`postprocess_response`** — extracts the `cost` field from the usage object (OpenRouter-specific, not part of the standard format) and adds `openrouter.model` and `openrouter.refusal` to the item metadata

```rust
impl CompletionsProvider for OpenRouterProvider {
    type Config = OpenRouterRequestConfig;

    fn provider_name(&self) -> &str { "OpenRouter" }
    fn endpoint_url(&self) -> &str { &self.base_url }
    fn config(&self) -> &OpenRouterRequestConfig { &self.request_config }

    fn preprocess_request(
        &self,
        builder: agentkit_http::HttpRequestBuilder,
    ) -> agentkit_http::HttpRequestBuilder {
        let mut builder = builder.bearer_auth(&self.api_key);
        if let Some(app_name) = &self.app_name {
            builder = builder.header("X-Title", app_name);
        }
        if let Some(site_url) = &self.site_url {
            builder = builder.header("HTTP-Referer", site_url);
        }
        builder
    }

    fn preprocess_response(
        &self,
        _status: StatusCode,
        body: &str,
    ) -> Result<(), LoopError> {
        if let Ok(e) = serde_json::from_str::<ErrorResponse>(body) {
            return Err(LoopError::Provider(format!(
                "OpenRouter returned error (code {}): {}",
                e.error.code, e.error.message
            )));
        }
        Ok(())
    }

    fn postprocess_response(
        &self,
        usage: &mut Option<Usage>,
        metadata: &mut MetadataMap,
        raw_response: &Value,
    ) {
        if let Some(cost) = raw_response.pointer("/usage/cost").and_then(Value::as_f64) {
            if let Some(usage) = usage {
                usage.cost = Some(CostUsage {
                    amount: cost,
                    currency: "USD".into(),
                    provider_amount: None,
                });
            }
        }
        if let Some(model) = raw_response.get("model").and_then(Value::as_str) {
            metadata.insert("openrouter.model".into(), Value::String(model.into()));
        }
        if let Some(refusal) = raw_response
            .pointer("/choices/0/message/refusal")
            .and_then(Value::as_str)
        {
            metadata.insert("openrouter.refusal".into(), Value::String(refusal.into()));
        }
    }
}
```

Using it:

```rust
let adapter = OpenRouterAdapter::new(
    OpenRouterConfig::new("sk-or-v1-...", "anthropic/claude-sonnet-4")
        .with_temperature(0.0)
        .with_max_completion_tokens(4096)
        .with_app_name("my-agent"),
)?;

let agent = Agent::builder()
    .model(adapter)
    .input(vec![Item::text(
        ItemKind::User,
        "Explain quicksort in one sentence.",
    )])
    .build()?;
```

Without tools or a loop, an agent can be used for a single one-shot inference call — send a message, get a response. Preload the user message via `AgentBuilder::input` so the first `next()` dispatches the model directly:

```rust
let mut driver = agent
    .start(SessionConfig::new("one-shot").with_cache(
        PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
    ))
    .await?;

if let LoopStep::Finished(result) = driver.next().await? {
    for item in result.items {
        for part in &item.parts {
            if let Part::Text(text) = part {
                println!("{}", text.text);
            }
        }
    }
}
```

The `cache` field is the session-level prompt caching policy — request-level configuration, not transcript data. See [Chapter 15: Prompt caching](./ch15-caching.md) for the full cache request shape, provider mapping, and per-turn overrides.

No tools are registered, so the model returns text and the driver finishes after a single turn. This is the simplest way to use agentkit — a typed HTTP client for chat completions with provider abstraction. The agent loop, covered in the next chapter, adds tool execution and iteration on top.

## Available providers

agentkit ships the following provider crates, all built on `agentkit-adapter-completions`:

| Crate                                                                                                                 | Provider                              | Auth                    | Default endpoint                          |
| --------------------------------------------------------------------------------------------------------------------- | ------------------------------------- | ----------------------- | ----------------------------------------- |
| [`agentkit-provider-openrouter`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-openrouter) | [OpenRouter](https://openrouter.ai)   | Bearer + custom headers | `openrouter.ai/api/v1/chat/completions`   |
| [`agentkit-provider-openai`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-openai)         | [OpenAI](https://platform.openai.com) | Bearer                  | `api.openai.com/v1/chat/completions`      |
| [`agentkit-provider-ollama`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-ollama)         | [Ollama](https://ollama.com)          | none                    | `localhost:11434/v1/chat/completions`     |
| [`agentkit-provider-vllm`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-vllm)             | [vLLM](https://docs.vllm.ai)          | optional Bearer         | `localhost:8000/v1/chat/completions`      |
| [`agentkit-provider-groq`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-groq)             | [Groq](https://groq.com)              | Bearer                  | `api.groq.com/openai/v1/chat/completions` |
| [`agentkit-provider-mistral`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-provider-mistral)       | [Mistral](https://mistral.ai)         | Bearer                  | `api.mistral.ai/v1/chat/completions`      |

Each follows the same pattern: a config struct with `new()` fluent builders (and an optional `from_env()` helper), a `Serialize` request config, and a `CompletionsProvider` impl. Provider-specific parameters are strongly typed — Ollama has `num_predict` and `top_k`, Mistral uses `max_tokens` instead of `max_completion_tokens`, OpenAI has `frequency_penalty` and `presence_penalty`.

For providers not listed here, you can either:

1. **Implement `CompletionsProvider`** if the provider speaks the OpenAI chat completions format (~50 lines)
2. **Implement `ModelAdapter` / `ModelSession` / `ModelTurn` directly** if the provider has a non-standard API (as shown in the AcmeAI example)

[Chapter 2: What is an agent loop? →](./ch02-what-is-an-agent-loop.md)
