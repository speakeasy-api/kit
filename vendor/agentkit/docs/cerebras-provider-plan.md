# agentkit-provider-cerebras — Integration Plan

Target: absolute parity with Cerebras Inference API. Bespoke client (no generic completions adapter reuse). Experimental/preview features gated behind Cargo features. Modelled after `agentkit-provider-anthropic` (dual buffered + streaming turn, SSE state machine, rich config builder, opaque `Value` passthrough for forward compat).

Source of truth:

- https://inference-docs.cerebras.ai/llms.txt (index)
- https://inference-docs.cerebras.ai/api-reference/chat-completions
- https://inference-docs.cerebras.ai/capabilities/{tool-use,structured-outputs,reasoning,streaming,payload-optimization,prompt-caching,predicted-outputs,service-tiers,batch}
- https://inference-docs.cerebras.ai/api-reference/versions
- https://inference-docs.cerebras.ai/support/{rate-limits,error}
- https://github.com/Cerebras/cerebras-cloud-sdk-{python,node}

---

## 1. Feature surface enumeration

### 1.1 Transport

| Aspect                   | Detail                                                                                 |
| ------------------------ | -------------------------------------------------------------------------------------- |
| Base URL                 | `https://api.cerebras.ai/v1` (override-able; dedicated endpoints allowed)              |
| Auth                     | `Authorization: Bearer <CEREBRAS_API_KEY>`                                             |
| Version header           | `X-Cerebras-Version-Patch: <n>` (opt-in to new major; 6-month overlap window)          |
| User-Agent               | crate-version stamped                                                                  |
| Compression in           | `Content-Type: application/vnd.msgpack` and/or `Content-Encoding: gzip` (request only) |
| Compression out          | Not supported                                                                          |
| Rate-limit headers       | `x-ratelimit-{limit,remaining,reset}-{requests-day,tokens-minute}`                     |
| Retryable status         | 408, 409, 429, 5xx                                                                     |
| Undocumented passthrough | SDKs expose `extra_headers/query/body`; we expose the equivalent                       |

### 1.2 Endpoints (scope of parity)

Scope: inference adapter for an agentic loop. In scope = anything whose output is chat-completions inference or directly supports it.

**In scope:**

- `POST /v1/chat/completions` — core turn loop.
- `GET /v1/models`, `GET /v1/models/{id}` — model discovery/validation (tiny, unconditional).
- Batch (feature=`batch`): `POST /v1/batches`, `GET /v1/batches`, `GET /v1/batches/{id}`, `POST /v1/batches/{id}/cancel` — async bulk chat-completions inference. Same endpoint in request bodies, same params, same response shape; different execution model. Justified in §8.1: reuses the chat-request builder so correctness logic is shared across interactive and bulk paths.
- Files (feature=`batch`): `POST|GET|DELETE /v1/files`, `GET /v1/files/{id}`, `GET /v1/files/{id}/content` — exists solely to feed batch (`purpose="batch"`). Folded into the `batch` feature; no independent surface.

**Out of scope:**

- `POST /v1/completions` (legacy text completions) — not chat-shaped, not agentic. Cerebras-specific params (`grammar_root`, `return_raw_tokens`, `suffix`, `best_of`, `echo`, `n`, polymorphic token-ID prompts) follow.
- Customer Management / Dedicated Endpoints (deploy model versions, aliases, list endpoints) — control plane. Deployed endpoints are reachable via `CerebrasConfig::with_base_url`; the inference path is already covered by chat-completions.

### 1.3 Chat-completions parameters (every field)

Headers: `Content-Type`, `Content-Encoding`, `X-Cerebras-Version-Patch`, `queue_threshold` (50–20000 ms, `flex`/`auto` tiers only — **private preview**).

Body:

- **Core:** `model`, `messages[]` (`system`|`user`|`assistant`|`developer`|`tool`), `max_completion_tokens`, `stream`, `stop`, `seed`, `user`, `temperature`, `top_p`, `frequency_penalty`, `presence_penalty`, `logit_bias`, `logprobs`, `top_logprobs`.
- **Tools:** `tools[]` (`function` w/ `name`, `description`, `parameters`, `strict`), `tool_choice` (`"none"` | `"auto"` | `"required"` | `{type: "function", function: {name}}`), `parallel_tool_calls`.
- **Structured output:** `response_format` = `{type:"text"}` | `{type:"json_object"}` | `{type:"json_schema", json_schema:{strict, schema}}`. Constraints: root-object, `additionalProperties:false`, ≤5000 chars, ≤10 nest depth, ≤500 props, ≤500 enum values. No recursive, external `$ref`, `pattern`, `format`, `minItems/maxItems`.
- **Reasoning:** `reasoning_effort` (`low`|`medium`|`high` for gpt-oss; `none` for glm-4.7), `clear_thinking` (glm-4.7 only), `reasoning_format` (`parsed`|`raw`|`hidden`|`none`), `disable_reasoning` (deprecated 2026-07-21).
- **Predicted outputs:** `prediction: {type:"content", content}`. Disables `tools`, `logprobs`, `n>1`.
- **Service tier (private preview):** `service_tier` ∈ `priority`|`default`|`auto`|`flex`.

### 1.4 Response shape

- Envelope: `id`, `object`, `created`, `model`, `system_fingerprint`, `service_tier`, `service_tier_used`, `choices[]`, `usage`, `time_info`.
- Choice: `index`, `finish_reason` ∈ `stop`|`length`|`content_filter`|`tool_calls`|`done`, `message`, `logprobs`, `reasoning_logprobs`.
- Message: `role`, `content`, `reasoning`, `tool_calls[]{id, type:"function", function:{name, arguments}}`.
- Usage: `prompt_tokens`, `completion_tokens`, `total_tokens`, `prompt_tokens_details.cached_tokens`, `completion_tokens_details.{accepted,rejected}_prediction_tokens`, `completion_tokens_details.reasoning_tokens`.
- `time_info`: `queue_time`, `prompt_time`, `completion_time`, `total_time`, `created`.

### 1.5 Streaming

SSE. Deltas stream `content`, `reasoning`, `tool_calls[].function.{name,arguments}` fragments. Terminal chunk carries `finish_reason:"done"` together with `usage` + `time_info`. No keep-alive/heartbeat documented — treat unknown events as ignorable.

### 1.6 Prompt caching

Fully automatic on `gpt-oss-120b`, `qwen-3-235b-a22b-instruct-2507`, `zai-glm-4.7`. Surfaces only through `usage.prompt_tokens_details.cached_tokens`. No request-side knob. 128-token block, 5-min TTL (up to 1 h). → we plumb the read-side metric; no write-side API.

### 1.7 Rate-limit + error

Standard OpenAI-style `{error: {message, type, code}}` (inferred; kept opaque-tolerant). Exceptions ladder: 400 BadRequest · 401 Auth · 402 PaymentRequired · 403 PermissionDenied · 404 NotFound · 408 Timeout · 422 Unprocessable · 429 RateLimit · 500 InternalServer · 503 ServiceUnavailable.

---

## 2. Crate layout

```
crates/agentkit-provider-cerebras/
  Cargo.toml
  README.md
  src/
    lib.rs               adapter/session/turn plumbing, auth, header assembly, send-request orchestration
    config.rs            CerebrasConfig + builder, env bootstrap, enums for every knob
    error.rs             CerebrasError, BuildError, ResponseError
    request.rs           transcript -> Chat Completions body; tool-spec synth; response_format; reasoning; prediction
    response.rs          buffered JSON -> VecDeque<ModelTurnEvent>; usage + time_info; reasoning; tool_calls
    sse.rs               raw `event:`/`data:` framer — same shape as anthropic's. Cerebras preserves `event:` lines: unnamed frames carry deltas, `event: error` frames carry fatal errors, `data: [DONE]` is the terminator (see §5.2)
    stream.rs            EventTranslator — per-choice state for content/reasoning/tool-call assembly across deltas
    media.rs             multimodal content-array encoding (future; gated where needed)
    reasoning.rs         ReasoningConfig + ReasoningFormat enums
    tool_choice.rs       ToolChoice enum mirroring API
    response_format.rs   OutputFormat { Text, JsonObject, JsonSchema{strict, schema} }
    service_tier.rs      ServiceTier enum + queue_threshold (feature = "service-tiers")
    prediction.rs        Prediction { Content(String) } (feature = "predicted-outputs")
    compression.rs       msgpack + gzip request-body encoder (feature = "compression")
    version.rs           X-Cerebras-Version-Patch helper
    rate_limit.rs        parse x-ratelimit-* into a RateLimitSnapshot returned on the adapter
    models.rs            GET /v1/models typed wrapper (unconditional)
    files.rs             Files API (feature = "batch"; exists solely to feed Batch)
    batch.rs             Batch API (feature = "batch"; async bulk chat-completions inference)
```

**Out of scope** (separate execution models / control-plane concerns — not `ModelAdapter`-shaped):

- `/v1/completions` (legacy text) — not chat-shaped, not agentic.
- Customer Management / Dedicated Endpoints — control plane. Deployed endpoints are reachable via `CerebrasConfig::with_base_url`; the inference path is already covered.

### 2.1 Cargo features

```toml
[features]
default = ["streaming"]
streaming          = []                           # on by default, mirrors anthropic
compression        = ["dep:flate2", "dep:rmp-serde"]
predicted-outputs  = []                           # preview but stable wire format
service-tiers      = []                           # private preview — gated
batch              = ["dep:reqwest/multipart"]    # async bulk chat-completions inference (includes Files API — batch requires file upload)
# Umbrella:
experimental       = ["service-tiers", "predicted-outputs", "batch"]
```

Rationale: user called out compression specifically, and "experimental features behind feature flags". Granular flags let downstream crates opt in surgically; `experimental` is a convenience union. Files API folds into `batch` because it exists only to feed it — no independent use case.

### 2.2 Dependencies

- `agentkit-core`, `agentkit-http`, `agentkit-loop`, `agentkit-tools-core` (workspace paths, identical to anthropic)
- `async-trait`, `futures-util`, `reqwest`, `serde`, `serde_json`, `thiserror`
- Optional: `flate2` (gzip), `rmp-serde` (msgpack), `base64` (only if multimodal parts land)

---

## 3. Public API surface

Exported from `lib.rs`:

```rust
pub struct CerebrasAdapter { /* Http + Arc<CerebrasConfig> */ }

pub use config::{
    CerebrasConfig,
    DEFAULT_BASE_URL,                // "https://api.cerebras.ai/v1"
    DEFAULT_VERSION_PATCH,           // None — unversioned until user opts in
    OutputFormat,
    ToolChoice,
    ReasoningConfig, ReasoningFormat, ReasoningEffort,
};
pub use error::{CerebrasError, BuildError, ResponseError};
pub use rate_limit::RateLimitSnapshot;

#[cfg(feature = "predicted-outputs")] pub use prediction::Prediction;
#[cfg(feature = "service-tiers")]     pub use service_tier::{ServiceTier, QueueThreshold};
#[cfg(feature = "compression")]       pub use compression::{RequestEncoding, CompressionConfig};
#[cfg(feature = "batch")]             pub use files::{FilesClient, FilePurpose, FileObject};
#[cfg(feature = "batch")]             pub use batch::{BatchClient, BatchJob, BatchStatus};
```

Trait impls: `ModelAdapter` → `CerebrasSession` → `CerebrasTurn` (buffered | streaming), identical shape to anthropic.

`BatchClient` is **not** a `ModelAdapter` impl. Batch's execution model (submit JSONL, poll to completion, match results by `custom_id`) does not fit the turn-loop trait shape. It is a separate surface that reuses the crate's chat-request builder (`request::build_chat_body`) to construct each JSONL line — single source of truth for Cerebras request shape across interactive and bulk inference.

---

## 4. Request construction (`request.rs`)

Single entry: `build_chat_body(cfg: &CerebrasConfig, turn: &TurnRequest) -> Result<Value, BuildError>`.

Pipeline:

1. **Roles.** Walk transcript, map agentkit `MessagePart`s → OpenAI-style `{role, content}`. `system`/`developer` messages stay top-of-list. Tool results become `{role:"tool", tool_call_id, content}`.
2. **Content.** Text → string. Structured parts → content-array (`[{type:"text",text}, {type:"image_url",image_url:{url}}]`) when multimodal lands (currently: text-only; non-text part → `BuildError::UnsupportedPart`).
3. **Tools.** `agentkit_tools_core::ToolSpec` → `{type:"function", function:{name, description, parameters, strict: cfg.tool_strict}}`. Name validated against `^[a-zA-Z0-9_-]{1,64}$`.
4. **Tool choice.** `cfg.tool_choice.to_json()` when set; omit otherwise so server default applies (`auto` w/ tools, else `none`).
5. **Response format.** `cfg.output_format.to_json()`; validate schema against documented constraints (length, depth, prop count, no `$ref` outside `$defs`, no `pattern`/`format`). Surface `BuildError::SchemaViolation` with the specific rule.
6. **Reasoning.** Apply `cfg.reasoning.to_body(&cfg.model)` — encodes `reasoning_effort`, `reasoning_format`, `clear_thinking`, `disable_reasoning` honouring model-specific whitelists. Unknown-model fallthrough = pass through verbatim (forward compat).
7. **Prediction** (feature=`predicted-outputs`): `cfg.prediction.to_json()`. Conflict-check: if `tools` non-empty or `logprobs==true` → `BuildError::PredictionConflicts`.
8. **Sampling / stop / penalties / logprobs / seed / user / `min_tokens`.**
9. **Service tier** (feature=`service-tiers`): `service_tier` in body, `queue_threshold` in header (collected separately).
10. **`stream: cfg.streaming`** (bool, always emitted — explicit > implicit).

Output: `(body: Value, extra_headers: Vec<(&'static str, String)>)` so header-bearing knobs (`queue_threshold`, `X-Cerebras-Version-Patch`) feed back to `lib.rs` for attachment.

### 4.1 Compression (`compression.rs`, feature-gated)

```rust
pub enum RequestEncoding { Json, Msgpack, JsonGzip, MsgpackGzip }

pub struct CompressionConfig {
    pub encoding: RequestEncoding,
    pub min_bytes: usize,   // skip compression for tiny payloads (docs: under a few KB overhead > benefit)
}
```

`encode_body(&Value, &CompressionConfig) -> (Bytes, &'static str /* content-type */, Option<&'static str> /* content-encoding */)`.

Plumbed into `lib.rs`: when feature off, body is always `serde_json::to_vec` + `application/json`. When on and `cfg.compression` is `Some`, `encode_body` runs and headers are overridden. Compression applies only to requests.

---

## 5. Response handling

### 5.1 Buffered (`response.rs`)

`build_turn_from_response(json: &str) -> Result<VecDeque<ModelTurnEvent>, ResponseError>`:

1. Parse envelope, emit `ModelTurnEvent::Usage(Usage { tokens: Some(TokenUsage { input_tokens, output_tokens, reasoning_tokens, cached_input_tokens, cache_write_input_tokens: None }), cost: None, metadata })`. Non-core fields — `accepted_prediction_tokens`, `rejected_prediction_tokens`, `time_info.*`, `service_tier`, `service_tier_used`, `system_fingerprint` — ride in `metadata` under the `cerebras.` prefix (see §5.3). **No core change needed**: `TokenUsage` already carries `reasoning_tokens` and `cached_input_tokens` (`crates/agentkit-core/src/lib.rs:1098`), and `Usage.metadata: MetadataMap = BTreeMap<String, serde_json::Value>` is the designed passthrough for provider-specific telemetry.
2. For each choice (usually one): emit `ToolCall` events for each entry in `message.tool_calls[]`, a `Delta(CommitPart::Text{…})` for content, a `Delta(CommitPart::Reasoning{…})` for `message.reasoning`, then `Finished(ModelTurnResult { finish_reason })`.
3. `finish_reason` mapping: `stop`|`done` → `Completed`, `length` → `MaxTokens`, `tool_calls` → `ToolCalls`, `content_filter` → `Blocked`.

### 5.3 Metadata key convention

All Cerebras-injected keys on `Usage.metadata` are prefixed `cerebras.` to keep the namespace disjoint from other providers:

| Key                                   | Value                                                                                       |
| ------------------------------------- | ------------------------------------------------------------------------------------------- |
| `cerebras.accepted_prediction_tokens` | `u64`                                                                                       |
| `cerebras.rejected_prediction_tokens` | `u64`                                                                                       |
| `cerebras.time_info`                  | object: `{queue_time, prompt_time, completion_time, total_time, created}` (seconds, floats) |
| `cerebras.service_tier`               | requested tier string (when set)                                                            |
| `cerebras.service_tier_used`          | actual tier string (on `auto`)                                                              |
| `cerebras.system_fingerprint`         | string                                                                                      |

Consumers decode with `.get("cerebras.time_info").and_then(Value::as_object)`. Documented in the crate's README.

### 5.4 ResponseError variants

```rust
#[derive(Debug, thiserror::Error)]
pub enum ResponseError {
    /// Malformed or unexpected JSON / missing required field. Reserved for
    /// protocol-level breakage; not for server-reported errors.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Server-reported error surfaced mid-stream via `event: error` or an
    /// unnamed frame whose JSON carries a top-level `error` key.
    #[error("stream error ({status_code:?}): {message}")]
    StreamError {
        message: String,
        status_code: Option<u16>,
    },
}
```

`StreamError` is distinct from `Protocol` so callers can distinguish "the API told us to stop" from "we couldn't decode the wire."

### 5.2 Streaming (`stream.rs`)

Cerebras SSE mirrors OpenAI's delta format but preserves `event:` lines for the error channel. Confirmed against `cerebras-cloud-sdk-python/_streaming.py:59-87` and `cerebras-cloud-sdk-node/src/streaming.ts:35-72`.

**Per-frame dispatch:**

| Frame                                                                     | Action                                                                                                                                                   |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `data: [DONE]\n\n`                                                        | Translator marks done; outer loop terminates.                                                                                                            |
| `event: error\ndata: {json}\n\n`                                          | Parse `data`; extract `error.message` (+ `status_code` if present); emit `ResponseError::StreamError { message, status_code }` — terminal.               |
| Unnamed frame (`data: {json}\n\n`) whose JSON has a top-level `error` key | Same `StreamError` treatment — terminal. Captures the case where Cerebras emits `{"error": {...}, "status_code": 500}` mid-stream without a named event. |
| Unnamed frame with a chat-completion chunk                                | Feed to `EventTranslator` as a delta.                                                                                                                    |
| Any other named event (`event: foo` unknown)                              | Silently ignored — forward compat.                                                                                                                       |
| Malformed JSON in `data`                                                  | `ResponseError::Protocol(String)` — distinct from `StreamError`; reserved for protocol-level breakage, not server-reported errors.                       |

**`EventTranslator` state per choice index:**

```rust
struct ChoiceState {
    content_open: bool,
    reasoning_open: bool,
    tool_calls: BTreeMap<u32 /* delta index */, ToolCallAccum>,
    finish_reason: Option<FinishReason>,
}
```

**Delta translation:**

- `delta.content` → append to buffer, flush on block close or `finish_reason`.
- `delta.reasoning` → separate buffer, flushed as `Reasoning` commit part.
- `delta.tool_calls[i]` → accumulate `name` and `arguments` JSON fragments by `index`; emit `ModelTurnEvent::ToolCall` once arguments parse cleanly at finish.
- Final chunk carries `finish_reason:"done"` + `usage` + `time_info` → emit `Usage` + `Finished`.
- Unknown delta fields → silently ignored (forward compat).

---

## 6. Config shape (`config.rs`)

```rust
#[derive(Clone)]
pub struct CerebrasConfig {
    // auth & transport
    pub api_key: String,
    pub base_url: String,                          // default: https://api.cerebras.ai/v1
    pub version_patch: Option<u32>,                // X-Cerebras-Version-Patch
    pub extra_headers: Vec<(String, String)>,      // SDK-style passthrough
    pub extra_body: Option<Value>,                 // merged into body — forward compat

    // model
    pub model: String,
    pub max_completion_tokens: Option<u32>,
    pub min_tokens: Option<i32>,                   // signed: -1 = max-seq-length sentinel

    // sampling
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub stop: Option<Vec<String>>,
    pub seed: Option<i64>,
    pub logit_bias: Option<BTreeMap<String, i32>>,
    pub logprobs: Option<bool>,
    pub top_logprobs: Option<u32>,
    pub user: Option<String>,

    // tools
    pub tool_choice: Option<ToolChoice>,
    pub parallel_tool_calls: Option<bool>,
    pub tool_strict: bool,                         // sets function.strict for each synthesised tool

    // output
    pub output_format: Option<OutputFormat>,

    // reasoning
    pub reasoning: Option<ReasoningConfig>,

    // stream
    pub streaming: bool,                           // default true

    // preview-gated
    #[cfg(feature = "predicted-outputs")] pub prediction: Option<Prediction>,
    #[cfg(feature = "service-tiers")]     pub service_tier: Option<ServiceTier>,
    #[cfg(feature = "service-tiers")]     pub queue_threshold_ms: Option<u32>,
    #[cfg(feature = "compression")]       pub compression: Option<CompressionConfig>,
}
```

Builder: one `with_*` per field (mirrors anthropic). `from_env()` reads `CEREBRAS_API_KEY` (required), `CEREBRAS_MODEL` (required), `CEREBRAS_BASE_URL`, `CEREBRAS_VERSION_PATCH`, `CEREBRAS_MAX_COMPLETION_TOKENS`.

### 6.1 Validation (constructor / builder)

- `api_key` non-empty.
- `top_logprobs` ∈ 0..=20; requires `logprobs == Some(true)`.
- `stop.len() ≤ 4`.
- `temperature` ∈ 0.0..=2.0.
- `frequency_penalty`/`presence_penalty` ∈ -2.0..=2.0.
- `min_tokens >= -1` (`-1` sentinel = max-seq-length).
- `queue_threshold_ms` ∈ 50..=20000.
- Schema constraints when `OutputFormat::JsonSchema { strict:true }`.
- `Prediction` incompatibilities (tools present, logprobs, n>1).
- Each rule surfaces as a distinct `BuildError` variant so callers can branch.

---

## 7. lib.rs plumbing (where it differs from anthropic)

1. Auth: single path (`Authorization: Bearer`). No dual api-key/bearer branching.
2. Header assembly order, collected in one helper:
   - `Authorization`
   - `Content-Type` (json / vnd.msgpack from `compression.rs`)
   - `Content-Encoding` (gzip when applicable)
   - `X-Cerebras-Version-Patch` if set
   - `queue_threshold` if set
   - `User-Agent: agentkit-provider-cerebras/<pkg-ver>`
   - `Accept: text/event-stream` when `cfg.streaming`
   - Any `extra_headers`
3. Rate-limit snapshot: after every response, parse `x-ratelimit-*` into `RateLimitSnapshot` and expose the last snapshot via `CerebrasAdapter::last_rate_limit()` (behind `Arc<Mutex<Option<_>>>`). No new events emitted — opt-in read.
4. Retry: **not the adapter's job.** `agentkit-http` is a trait façade with no retry layer. Callers wanting retry build their `Http` over the `reqwest-middleware-client` feature with `reqwest-retry` (handles 408/409/429/5xx out of the box) and pass it via `CerebrasAdapter::with_client(config, http)` — same pattern as `AnthropicAdapter::with_client` (`crates/agentkit-provider-anthropic/src/lib.rs:88`). Documented in README.
5. Non-streaming + streaming branching identical to anthropic: buffered returns `VecDeque<ModelTurnEvent>`; streaming returns a boxed state machine.
6. Cancellation: same `futures::select!` race on body chunks as anthropic.

---

## 8. Ancillary endpoints (not on the turn path)

Surfaces exposed on `CerebrasAdapter` alongside the `ModelAdapter` trait impl:

```rust
impl CerebrasAdapter {
    pub fn models(&self) -> ModelsClient<'_> { … }                      // unconditional
    #[cfg(feature = "batch")] pub fn batches(&self) -> BatchClient<'_> { … }
    #[cfg(feature = "batch")] pub fn files(&self)   -> FilesClient<'_> { … }
}
```

`ModelsClient` is tiny and unconditional — reads `/v1/models` and `/v1/models/{id}`, useful for validating a configured model at startup.

### 8.1 BatchClient + FilesClient (feature=`batch`)

Batch is async bulk chat-completions inference: upload a JSONL where each line is a complete `/v1/chat/completions` request, submit, poll to terminal, fetch the output file. Cerebras batch supports only `/v1/chat/completions` in request bodies — same params, same response shape as the turn-loop path. Files API exists only to feed batch (`purpose="batch"`); no independent consumer.

**Why it belongs in the crate despite not being turn-shaped.** The crate's value-add is correct Cerebras chat-request construction — reasoning-per-model rules, strict-schema validation, predicted-output conflict checks, compression encoding, every `CerebrasConfig` knob. A user doing batch by hand has to re-implement all of that. Keeping batch here means the **request builder is shared source of truth** across interactive and bulk inference.

**Request reuse.** `BatchClient::submit_chat_batch<I>(items: I)` accepts an iterator of `(custom_id, TurnRequest, Option<ChatOverrides>)` and serialises each line via the same `request::build_chat_body` used by the turn loop. Per-line config overrides (e.g. different `reasoning_effort`, `response_format`, `max_completion_tokens`) are applied via a small `ChatOverrides` struct that layers onto the adapter's base `CerebrasConfig`.

**Types:**

```rust
pub struct FilesClient<'a> { /* &Http, &CerebrasConfig */ }
pub struct BatchClient<'a> { /* &Http, &CerebrasConfig */ }

pub enum FilePurpose { Batch }                          // only purpose in scope

pub struct FileObject {
    pub id: String,
    pub bytes: u64,
    pub created_at: u64,
    pub filename: String,
    pub purpose: FilePurpose,
}

pub struct BatchJob {
    pub id: String,
    pub status: BatchStatus,
    pub endpoint: String,                               // "/v1/chat/completions" — only supported value
    pub input_file_id: String,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub request_counts: BatchRequestCounts,             // {total, completed, failed}
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub finalizing_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub cancelled_at: Option<u64>,
    pub failed_at: Option<u64>,
    pub metadata: BTreeMap<String, String>,
}

pub enum BatchStatus {
    Validating, InProgress, Finalizing,
    Completed, Failed, Expired,
    Cancelling, Cancelled,
}

pub struct ChatOverrides {
    pub model: Option<String>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning: Option<ReasoningConfig>,
    pub response_format: Option<OutputFormat>,
    // …per-line knobs; a strict subset of CerebrasConfig chat fields.
}
```

**Methods:**

```rust
impl<'a> FilesClient<'a> {
    pub async fn upload(&self, filename: &str, bytes: impl Into<Bytes>, purpose: FilePurpose) -> Result<FileObject, CerebrasError>;
    pub async fn list(&self) -> Result<Vec<FileObject>, CerebrasError>;
    pub async fn retrieve(&self, id: &str) -> Result<FileObject, CerebrasError>;
    pub async fn content(&self, id: &str) -> Result<BodyStream, CerebrasError>;
    pub async fn delete(&self, id: &str) -> Result<(), CerebrasError>;
}

impl<'a> BatchClient<'a> {
    /// Assembles JSONL via `request::build_chat_body` per line, uploads, and submits.
    /// Returns the created `BatchJob`.
    pub async fn submit_chat_batch<I>(&self, items: I, metadata: BTreeMap<String, String>)
        -> Result<BatchJob, CerebrasError>
    where I: IntoIterator<Item = (String /* custom_id */, TurnRequest, Option<ChatOverrides>)>;

    /// Lower-level: create a batch from an already-uploaded input_file_id.
    pub async fn create(&self, input_file_id: &str, metadata: BTreeMap<String, String>)
        -> Result<BatchJob, CerebrasError>;

    pub async fn list(&self) -> Result<Vec<BatchJob>, CerebrasError>;
    pub async fn retrieve(&self, id: &str) -> Result<BatchJob, CerebrasError>;
    pub async fn cancel(&self, id: &str) -> Result<BatchJob, CerebrasError>;

    /// Convenience: poll `retrieve` until a terminal status, then fetch output + error files.
    /// Caller provides poll interval; cancellation aborts the wait, not the batch.
    pub async fn wait(&self, id: &str, interval: Duration, cancel: Option<TurnCancellation>)
        -> Result<BatchOutcome, CerebrasError>;
}

pub struct BatchOutcome {
    pub job: BatchJob,                                  // final state
    pub outputs: Option<BodyStream>,                    // output_file content stream (unordered — match by custom_id)
    pub errors:  Option<BodyStream>,                    // error_file content stream
}
```

**Explicitly not a `ModelAdapter`/`ModelSession`/`ModelTurn` impl.** Batch's execution model is submit-and-poll; there is no streaming, no mid-turn tool calls, no cancellation of an in-flight completion. Forcing it into `ModelTurnEvent` would be a lie. `BatchOutcome` returns raw JSONL streams — the caller matches results to their inputs by `custom_id`.

**Validation** (on `submit_chat_batch` input):

- File size projection ≤ 200 MB (docs limit).
- ≤ 50,000 items per batch.
- Each serialised line ≤ 1 MB.
- Every chat body runs through the same `build_chat_body` validator used by the turn loop — so preview-feature conflicts (e.g. `prediction` + `tools`) fail at submit time, not after batch processing.

**Result ordering.** Documented explicitly in the docstring: results in `output_file` are unordered; callers must match via the `custom_id` they supplied. `BatchOutcome::outputs` is streamed raw, not parsed into a map, because 50K-item batches would blow memory if we buffered.

---

## 9. Testing strategy

Mirror the anthropic layout; place tests next to the code they cover.

- `config.rs` — constructor rejections (every `BuildError`).
- `request.rs` —
  - transcript → body snapshot for: plain chat, with tools, with `strict` JSON schema, with reasoning (each model), with prediction, with system+developer messages, with tool results.
  - schema-constraint violations fire the right error.
  - prediction+tools conflict fires `PredictionConflicts`.
- `response.rs` — parse fixtures for content-only, tool-calls, reasoning, cached-tokens, predicted-output tokens, time_info, every `finish_reason`.
- `sse.rs` — framer-level coverage, **required** (every row must have a test):

  | Case                                                                            | Test asserts                                                                               |
  | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
  | Single complete frame `event: error\ndata: {...}\n\n`                           | Both `event` name and `data` preserved verbatim                                            |
  | Single unnamed frame `data: {...}\n\n`                                          | `event` is `None`, `data` preserved                                                        |
  | Terminator `data: [DONE]\n\n`                                                   | Yielded as an ordinary frame; sentinel interpretation lives in `stream.rs`, not the framer |
  | Frame split across chunk boundary mid-`data:` (e.g. `data: {"` → `foo":1}\n\n`) | Reassembled correctly, no loss                                                             |
  | Frame split on the terminating `\n\n` (e.g. `...}` → `\n\n`)                    | Single frame emitted                                                                       |
  | `\r\n\r\n` line endings                                                         | Accepted; single frame emitted                                                             |
  | Multiple `data:` lines within one frame                                         | Joined with `\n` per SSE spec                                                              |
  | Trailing buffer without terminator                                              | Not emitted until the next chunk completes the frame                                       |
  | Unknown prefix lines (`id:`, `retry:`, comment `:`)                             | Ignored, do not corrupt frame parsing                                                      |

- `stream.rs` — full decode→translate pipeline, **required** (every row must have a test):

  | Case                                                                                  | Test asserts                                                                                                                |
  | ------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
  | Text-only deltas → `finish_reason:"done"` with `usage` + `time_info`                  | `Delta(Text)` flushed, terminal `Usage` + `Finished(Completed)`, `time_info` in `Usage.metadata` under `cerebras.time_info` |
  | Tool-call streaming (name arrives, then argument JSON in fragments)                   | Fragments accumulate per `index`; exactly one `ToolCall` emitted per index when the JSON parses                             |
  | Reasoning-then-content deltas                                                         | `Delta(Reasoning)` precedes `Delta(Text)`; both committed                                                                   |
  | `prompt_tokens_details.cached_tokens` present in final usage                          | Surfaced as `TokenUsage.cached_input_tokens`                                                                                |
  | `completion_tokens_details.{reasoning,accepted_prediction,rejected_prediction}`       | First → `TokenUsage.reasoning_tokens`; last two → metadata under `cerebras.*`                                               |
  | `event: error\ndata: {"error":{"message":"boom"},"status_code":429}`                  | Surfaces `ResponseError::StreamError { message:"boom", status_code: Some(429) }` — terminal                                 |
  | Unnamed frame `data: {"error":{"message":"oops"}}` (no `event:` line)                 | Same `StreamError`; confirms the unnamed-error path                                                                         |
  | Unknown named event `event: heartbeat\ndata: {}`                                      | Silently ignored, stream continues                                                                                          |
  | Malformed JSON in a `data:` frame                                                     | `ResponseError::Protocol(..)` — distinct from `StreamError`                                                                 |
  | `finish_reason` mapping: `stop` / `done` / `length` / `tool_calls` / `content_filter` | Mapped to `Completed` / `Completed` / `MaxTokens` / `ToolCalls` / `Blocked` respectively                                    |

- `compression.rs` (feature=compression) — encode roundtrips, min_bytes threshold, header selection.
- `lib.rs` — header aggregation, cancellation race (pre-fire + mid-stream), retry classification.
- Integration example under `examples/` that runs against a mock server with canned SSE.

No live-API tests in CI. README notes how to set `CEREBRAS_API_KEY` for ad-hoc runs.

---

## 10. Known open questions (flag before coding)

1. ~~**Does `agentkit-loop::ModelTurnEvent::Usage` carry `cached_tokens` / `accepted_prediction_tokens` / `reasoning_tokens` today?**~~ **Resolved.** Core `TokenUsage` already carries `reasoning_tokens` + `cached_input_tokens`; Cerebras-specific predicted-output tokens and `time_info` ride on `Usage.metadata` under the `cerebras.` prefix (see §5.3). Matches the `anthropic.*` convention already used in `agentkit-provider-anthropic`. No core change required.
2. ~~**Does the existing retry middleware in `agentkit-http` already handle 408/409/429/5xx?**~~ **Not applicable.** `agentkit-http` is a trait façade without a retry layer; retry is a consumer concern. Callers opt into `reqwest-middleware-client` + `reqwest-retry` and pass the configured client via `CerebrasAdapter::with_client`.
3. ~~**SSE error-frame format.**~~ **Resolved.** Confirmed from `cerebras-cloud-sdk-python/_streaming.py:59-87` and `cerebras-cloud-sdk-node/src/streaming.ts:35-72`: two terminal error shapes — `event: error\ndata: {"error": {...}}` and unnamed `data: {"error": {...}, "status_code": ...}`. Terminator is `data: [DONE]`. Handled in §5.2, error type in §5.4, test coverage enumerated in §9.
4. ~~**Grammar-based completions on `/v1/completions`.**~~ **Out of scope.** `/v1/completions` is not chat-shaped and not agentic — a user in an agent loop never hits this endpoint. Completions-only params (`grammar_root`, `return_raw_tokens`, `suffix`, `best_of`, `echo`, `n`, polymorphic token-ID prompts) are dropped. `min_tokens` survives as a first-class chat field because the SDK confirms it exists on `/v1/chat/completions` too (`chat/completion_create_params.py:111`); `reasoning_format` is already carried by `ReasoningConfig` for chat. Anyone needing legacy completions uses the Cerebras SDK directly or a separate crate.

---

## 11. Delivery shape (what the crate looks like when done)

- `crates/agentkit-provider-cerebras/` added to workspace members.
- Top-level `README.md` mentions Cerebras alongside other providers.
- Doc-comment quickstart matches the anthropic style (env-driven config, `Agent::builder().model(adapter).build()`).
- Every preview feature guarded by its own `#[cfg(feature = "...")]`; default build compiles to the turn-loop + compression-off path.
- No placeholders, no TODOs in merged code — all 10 feature areas in §1 implemented and tested.

### 11.1 Examples

Two workspace members under `examples/`, both `publish = false`, modelled on `examples/anthropic-chat/`. Registered in root `Cargo.toml:members` next to the existing anthropic/openrouter entries, and referenced from the top-level `README.md` provider-examples list.

**Design principle — no hidden knobs.** Every config value and wire-level toggle that `CerebrasConfig`, `BatchClient`, `FilesClient`, or `ModelsClient` supports must be independently verifiable by running the examples. A reviewer pulls the crate, sets `CEREBRAS_API_KEY`, and can exercise every feature surface through CLI flags or interactive REPL commands without reading source. Each knob below gets either a CLI flag, a REPL command, an env var, or a separate example binary.

#### 11.1.1 `examples/cerebras-chat/`

Interactive REPL. Primary demo of the turn-loop path. Deps: `agentkit-core`, `agentkit-loop`, `agentkit-provider-cerebras` built with `features = ["compression", "predicted-outputs", "service-tiers"]` — every chat-relevant feature on so the REPL can toggle any of them at runtime.

**CLI flags** — one per `CerebrasConfig` knob:

| Flag                                                             | `CerebrasConfig` field                                                                                     |
| ---------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `--model <id>`                                                   | `model`                                                                                                    |
| `--base-url <url>`                                               | `base_url` (default: `https://api.cerebras.ai/v1`)                                                         |
| `--version-patch <n>`                                            | `version_patch` (→ `X-Cerebras-Version-Patch`)                                                             |
| `--max-completion-tokens <n>`                                    | `max_completion_tokens`                                                                                    |
| `--min-tokens <n>`                                               | `min_tokens` (accepts `-1`)                                                                                |
| `--temperature <f>`                                              | `temperature`                                                                                              |
| `--top-p <f>`                                                    | `top_p`                                                                                                    |
| `--frequency-penalty <f>` / `--presence-penalty <f>`             | penalties                                                                                                  |
| `--stop <s>` (repeatable, max 4)                                 | `stop`                                                                                                     |
| `--seed <n>`                                                     | `seed`                                                                                                     |
| `--logit-bias <token_id>=<bias>` (repeatable)                    | `logit_bias`                                                                                               |
| `--logprobs` / `--top-logprobs <n>`                              | `logprobs`, `top_logprobs`                                                                                 |
| `--user <id>`                                                    | `user`                                                                                                     |
| `--tool-choice <auto\|none\|required\|tool:<name>>`              | `tool_choice`                                                                                              |
| `--no-parallel-tool-calls`                                       | `parallel_tool_calls = false`                                                                              |
| `--tool-strict`                                                  | `tool_strict = true`                                                                                       |
| `--response-format <text\|json_object\|json_schema:<path.json>>` | `output_format`                                                                                            |
| `--reasoning-effort <low\|medium\|high\|none>`                   | `reasoning.effort`                                                                                         |
| `--reasoning-format <parsed\|raw\|text_parsed\|hidden\|none>`    | `reasoning.format`                                                                                         |
| `--clear-thinking <bool>`                                        | `reasoning.clear_thinking` (glm-4.7)                                                                       |
| `--disable-reasoning`                                            | `reasoning.disable_reasoning`                                                                              |
| `--no-streaming`                                                 | `streaming = false`                                                                                        |
| `--prediction <path>`                                            | reads file, sets `Prediction::Content(...)`                                                                |
| `--service-tier <priority\|default\|auto\|flex>`                 | `service_tier`                                                                                             |
| `--queue-threshold-ms <n>`                                       | `queue_threshold_ms`                                                                                       |
| `--compression <none\|msgpack\|gzip\|msgpack+gzip>`              | `compression.encoding`                                                                                     |
| `--compression-min-bytes <n>`                                    | `compression.min_bytes`                                                                                    |
| `--extra-header <k>=<v>` (repeatable)                            | `extra_headers`                                                                                            |
| `--extra-body <path.json>`                                       | `extra_body` (deep-merged)                                                                                 |
| `--tool <name>=<path.json>` (repeatable)                         | registers a local tool from a JSON-schema file; REPL wires a pass-through handler so the model can call it |
| `--mcp-server <path>`                                            | attaches an MCP server whose tools appear in the session                                                   |

**REPL slash-commands** — for knobs that make more sense mid-session:

| Command               | Effect                                                                                                                                                                   |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `/set <flag> <value>` | Live-rebuild config for the _next_ turn (covers every CLI flag)                                                                                                          |
| `/show`               | Dumps effective `CerebrasConfig` as JSON (redacts api_key)                                                                                                               |
| `/usage`              | Prints last turn's `TokenUsage` + every `cerebras.*` metadata key (cached tokens, accepted/rejected prediction tokens, time_info, service_tier_used, system_fingerprint) |
| `/ratelimit`          | Prints `CerebrasAdapter::last_rate_limit()` (`x-ratelimit-*` snapshot)                                                                                                   |
| `/headers`            | Prints request headers used on the last turn (redacted auth) — verifies `Content-Type`, `Content-Encoding`, `X-Cerebras-Version-Patch`, `queue_threshold`                |
| `/cancel`             | Cancels the in-flight turn (verifies cancellation plumbing §7.6)                                                                                                         |
| `/new`                | Starts a fresh session                                                                                                                                                   |
| `/reset`              | Clears transcript, keeps config                                                                                                                                          |
| `/models`             | Calls `CerebrasAdapter::models().list()` — verifies unfeatured models endpoint                                                                                           |

**Env vars** — `CEREBRAS_API_KEY`, `CEREBRAS_MODEL`, `CEREBRAS_BASE_URL`, `CEREBRAS_VERSION_PATCH`, `CEREBRAS_MAX_COMPLETION_TOKENS` (§6 `from_env()`). CLI flags override env.

**Banner on startup**: prints every effective knob (redacted key) so a screenshot proves the run exercised the intended config. Mirrors `anthropic-chat`'s `print_banner` shape.

**Stream decoding**: the REPL prints `delta.content` incrementally, annotates `delta.reasoning` chunks in a distinct colour, shows tool-call assembly live, and prints the terminal `Usage`/`time_info`. Covers every `ModelTurnEvent` variant.

#### 11.1.2 `examples/cerebras-batch/`

One-shot binary. Covers Files + Batch — off the turn path but still inference. Proves the chat-request builder is the single source of truth across interactive and bulk inference.

Deps: `agentkit-provider-cerebras` with `features = ["batch", "compression", "predicted-outputs", "service-tiers"]`. The preview features are enabled so per-line `ChatOverrides` can exercise reasoning / response-format / prediction inside a batch.

Subcommands (verbs mirror the Cerebras API):

| Subcommand                                              | Calls                                                                                                                                                                     |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `files upload <path> --purpose batch`                   | `FilesClient::upload`                                                                                                                                                     |
| `files list`                                            | `FilesClient::list`                                                                                                                                                       |
| `files get <id>`                                        | `FilesClient::retrieve`                                                                                                                                                   |
| `files content <id>`                                    | `FilesClient::content` (streams to stdout)                                                                                                                                |
| `files delete <id>`                                     | `FilesClient::delete`                                                                                                                                                     |
| `batches create --input-file-id <id> [--metadata k=v]…` | `BatchClient::create`                                                                                                                                                     |
| `batches submit <prompts.json> [--overrides <path>]`    | `BatchClient::submit_chat_batch` — proves request-builder reuse by reading a JSON array of `{custom_id, messages, overrides?}` and serialising each via `build_chat_body` |
| `batches list`                                          | `BatchClient::list`                                                                                                                                                       |
| `batches get <id>`                                      | `BatchClient::retrieve`                                                                                                                                                   |
| `batches cancel <id>`                                   | `BatchClient::cancel`                                                                                                                                                     |
| `batches wait <id> [--poll-secs <n>]`                   | polls `retrieve` until terminal, prints each status transition, then fetches `output_file_id` / `error_file_id`                                                           |
| `run <prompts.json>`                                    | chains `submit` → `wait` → fetch — full happy path end-to-end                                                                                                             |

Every documented batch status (`validating`, `in_progress`, `finalizing`, `completed`, `failed`, `expired`, `cancelling`, `cancelled`) is surfaced in `wait` output so a reviewer sees the full state machine.

**Verification of request-builder reuse.** `batches submit` accepts an `--overrides <path>` flag where the JSON specifies a `ChatOverrides` per `custom_id`. The example prints the generated JSONL to stdout before uploading (behind `--show-jsonl`), so a reviewer can confirm that reasoning / response-format / predicted-output conflict rules apply identically to batch lines and turn-loop requests.

#### 11.1.3 What covers what (verification matrix)

| Config/feature                                                                           | Where demoed                                               |
| ---------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| Every `CerebrasConfig` field                                                             | `cerebras-chat` CLI flags + `/set` + `/show`               |
| Every `ReasoningConfig` variant                                                          | `cerebras-chat` `--reasoning-*` flags                      |
| Every `OutputFormat` variant                                                             | `cerebras-chat` `--response-format`                        |
| Every `ToolChoice` variant                                                               | `cerebras-chat` `--tool-choice`                            |
| Every `ServiceTier` variant + `queue_threshold_ms`                                       | `cerebras-chat` `--service-tier`, `--queue-threshold-ms`   |
| Every `RequestEncoding` variant + `min_bytes`                                            | `cerebras-chat` `--compression`, `--compression-min-bytes` |
| `Prediction::Content`                                                                    | `cerebras-chat` `--prediction <path>`                      |
| Streaming on/off                                                                         | `cerebras-chat` `--no-streaming`                           |
| Buffered + streaming tool-call accumulation                                              | `cerebras-chat` `--tool name=schema.json` + conversation   |
| Cancellation mid-stream                                                                  | `cerebras-chat` `/cancel`                                  |
| `cerebras.*` metadata surfacing                                                          | `cerebras-chat` `/usage`                                   |
| Rate-limit header snapshot                                                               | `cerebras-chat` `/ratelimit`                               |
| Version-patch header                                                                     | `cerebras-chat` `--version-patch` + `/headers`             |
| Extra-header / extra-body passthrough                                                    | `cerebras-chat` `--extra-header`, `--extra-body`           |
| `GET /v1/models` + `GET /v1/models/{id}`                                                 | `cerebras-chat` `/models`                                  |
| Files API (upload / list / retrieve / content / delete)                                  | `cerebras-batch files *`                                   |
| Batch API (create / submit / list / retrieve / cancel / poll-to-completion / full `run`) | `cerebras-batch batches *` and `cerebras-batch run`        |
| Every `BatchStatus` variant                                                              | `cerebras-batch batches wait` output transitions           |
| Request-builder reuse across turn + batch                                                | `cerebras-batch batches submit --show-jsonl`               |
| Per-line `ChatOverrides`                                                                 | `cerebras-batch batches submit --overrides <path>`         |

Anything not in this matrix is a gap — blocks merge.

Retain the §9 mock-SSE integration test — it's the CI fixture, not a user demo.
