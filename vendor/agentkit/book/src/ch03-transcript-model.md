# The transcript model

The transcript is the agent's memory of a conversation. Every message, tool call, tool result, and piece of context is represented as an `Item` in a `Vec<Item>`. The model sees the transcript on every turn. The loop appends to it. Compaction trims it.

This chapter covers `agentkit-core`: the foundational data types that every other crate depends on.

## The transcript as a data structure

A transcript is a flat vector of items. Each item has a role and carries content:

```text
Vec<Item>
├── Item { kind: System,     parts: [Text("You are a coding assistant.")] }
├── Item { kind: Context,    parts: [Text("Project uses Rust 2024 edition...")] }
├── Item { kind: User,       parts: [Text("Read src/main.rs")] }
├── Item { kind: Assistant,  parts: [Text("I'll read that file."),
│                                    ToolCall { name: "fs_read_file", ... }] }
├── Item { kind: Tool,       parts: [ToolResult { output: "fn main() {...}", ... }] }
└── Item { kind: Assistant,  parts: [Text("The file contains...")] }
```

This is the complete state that the model receives on every turn. The loop does not maintain hidden side channels or out-of-band context — if something affects the model's behaviour, it's in the transcript.

## Items and roles

An `Item` is the basic unit of the transcript:

```rust
pub struct Item {
    pub id: Option<MessageId>,
    pub kind: ItemKind,
    pub parts: Vec<Part>,
    pub metadata: MetadataMap,
}
```

The `kind` field determines the item's role:

```rust
pub enum ItemKind {
    System,      // Application-level instructions
    Developer,   // Developer-level instructions
    User,        // End-user messages
    Assistant,   // Model-generated responses
    Tool,        // Tool execution results
    Context,     // Loaded project context (AGENTS.md, skills, etc.)
}
```

The variants are ordered: `System < Developer < User < Assistant < Tool < Context`. This ordering is used by compaction strategies that need to sort or prioritise items by role.

Role mapping to provider wire formats:

| agentkit `ItemKind` | OpenAI role   | What it carries                    |
| ------------------- | ------------- | ---------------------------------- |
| `System`            | `"system"`    | Hardcoded application instructions |
| `Developer`         | `"system"`    | Developer-level instructions       |
| `User`              | `"user"`      | End-user messages                  |
| `Assistant`         | `"assistant"` | Model-generated text + tool calls  |
| `Tool`              | `"tool"`      | Tool execution results             |
| `Context`           | `"system"`    | Project context (AGENTS.md, etc.)  |

System, Developer, and Context all map to `"system"` in the OpenAI wire format, but they carry different semantic intent. The distinction matters for compaction: system items are never trimmed, context items may be refreshed, and developer items sit between the two. Collapsing them into a single kind would lose information that compaction strategies need.

### Why item-based, not message-based

Older chat APIs model conversations as a flat list of messages with a `role` field. agentkit uses "items" with "parts" instead, because modern models work with content blocks — a single assistant response may contain text, a tool call, reasoning output, and structured data. Flattening these into separate messages loses structure that the model, compaction strategies, and reporters all need.

```text
Flat message model (what you'd get with role + string):

  { role: "assistant", content: "I'll read main.rs" }
  { role: "assistant", content: null, tool_calls: [...] }

  Two "messages" for one logical response.
  Which one do you compact? How do you correlate them?


Item + parts model (agentkit):

  Item {
      kind: Assistant,
      parts: [
          Text("I'll read main.rs"),
          ToolCall { name: "fs_read_file", input: { "path": "src/main.rs" } },
      ]
  }

  One item. All parts belong to the same response.
  Compaction, reporting, and persistence all see one unit.
```

## Content parts

Each item contains one or more `Part` values:

```rust
pub enum Part {
    Text(TextPart),
    Media(MediaPart),
    File(FilePart),
    Structured(StructuredPart),
    Reasoning(ReasoningPart),
    ToolCall(ToolCallPart),
    ToolResult(ToolResultPart),
    Custom(CustomPart),
}
```

The part types cover the full range of content that flows through an agent:

| Part variant | Primary use                       | Example                       |
| ------------ | --------------------------------- | ----------------------------- |
| `Text`       | User messages, assistant replies  | `"Hello, world!"`             |
| `Media`      | Images, audio, video              | A PNG screenshot              |
| `File`       | File attachments                  | `report.csv`                  |
| `Structured` | JSON output, function returns     | `{ "status": "ok" }`          |
| `Reasoning`  | Chain-of-thought, thinking blocks | Model's internal reasoning    |
| `ToolCall`   | Model requests a tool invocation  | `fs_read_file("src/main.rs")` |
| `ToolResult` | Tool execution output             | `"fn main() { ... }"`         |
| `Custom`     | Provider-specific extensions      | Raw provider-specific content |

### Design decision: comprehensive multimodal from day one

agentkit ships with first-class support for text, audio, image, video, files, structured output, and reasoning blocks. The `Custom` variant exists as an escape hatch for provider-specific content, but the goal is that `Custom` should be rare — common modalities should map to named variants.

This matters because a text-only provider, a voice-only provider, and a multimodal provider all map naturally into the same `Item { parts: Vec<Part> }` structure. Complexity grows linearly with modalities, not combinatorially with provider combinations.

### Part type details

`TextPart` is the simplest and most common:

```rust
pub struct TextPart {
    pub text: String,
    pub metadata: MetadataMap,
}
```

`MediaPart` handles binary content through a modality discriminant and a data reference:

```rust
pub struct MediaPart {
    pub modality: Modality,    // Audio, Image, Video, Binary
    pub mime_type: String,     // e.g. "image/png", "audio/wav"
    pub data: DataRef,
    pub metadata: MetadataMap,
}
```

`ReasoningPart` captures model chain-of-thought output, which some providers expose alongside the final answer:

```rust
pub struct ReasoningPart {
    pub summary: Option<String>,   // Human-readable reasoning
    pub data: Option<DataRef>,     // Opaque reasoning data
    pub redacted: bool,            // Provider filtered the content
    pub metadata: MetadataMap,
}
```

The `redacted` flag is important: some providers expose reasoning in debug mode but redact it in production. The transcript records that reasoning happened even when the content is withheld.

`StructuredPart` carries validated JSON output:

```rust
pub struct StructuredPart {
    pub value: Value,
    pub schema: Option<Value>,     // JSON Schema the value conforms to
    pub metadata: MetadataMap,
}
```

### The `DataRef` abstraction

Media, files, and other binary content don't carry their bytes inline by default. Instead, they reference data through `DataRef`:

```rust
pub enum DataRef {
    InlineText(String),    // UTF-8 text (e.g. base64-encoded image)
    InlineBytes(Vec<u8>),  // Raw bytes
    Uri(String),           // External URL
    Handle(ArtifactId),    // Reference to an artifact store
}
```

This is a storage-agnostic pointer. The same `MediaPart` can reference an image as a base64 string (for small images going directly to the model), a URL (for provider-hosted content), or an artifact handle (for content managed by the host application).

```text
DataRef variants and when to use them:

InlineText ─── small payloads already base64-encoded
                (provider APIs often accept images this way)

InlineBytes ── small payloads in raw binary form
                (useful for local processing before encoding)

Uri ────────── content hosted externally
                (the provider fetches it, or the adapter does)

Handle ─────── content in a host-managed artifact store
                (transcript stays lightweight, data lives elsewhere)
```

This lets the transcript stay lightweight while supporting large payloads through external storage. A conversation with many image screenshots doesn't bloat the transcript if the images are stored as `Handle` references.

## Tool call and result types

Tool interaction is modeled as content parts, not side channels:

```rust
pub struct ToolCallPart {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
    pub metadata: MetadataMap,
}

pub struct ToolResultPart {
    pub call_id: ToolCallId,
    pub output: ToolOutput,
    pub is_error: bool,
    pub metadata: MetadataMap,
}
```

The `call_id` on `ToolResultPart` references the `id` on `ToolCallPart`. This correlation is how the model matches results back to the requests it made.

```text
Correlation between tool calls and results:

  Item { kind: Assistant, parts: [
      ToolCall { id: "call-1", name: "fs_read_file", input: {...} },
      ToolCall { id: "call-2", name: "shell_exec",   input: {...} },
  ]}
       │                                │
       │ call_id: "call-1"              │ call_id: "call-2"
       ▼                                ▼
  Item { kind: Tool, parts: [
      ToolResult { call_id: "call-1", output: "fn main()...", is_error: false },
      ToolResult { call_id: "call-2", output: "error: ...",   is_error: true  },
  ]}
```

When the model requests multiple tool calls in a single response, the assistant item contains multiple `ToolCallPart`s, and the corresponding tool item contains multiple `ToolResultPart`s. The `id`/`call_id` pairs maintain the mapping.

`ToolOutput` preserves rich structure:

```rust
pub enum ToolOutput {
    Text(String),
    Structured(Value),
    Parts(Vec<Part>),
    Files(Vec<FilePart>),
}
```

Tools don't have to collapse their output to a plain string. A tool that reads a file returns `Text`. A tool that queries a database returns `Structured`. A tool that captures a screenshot returns `Parts` containing a `MediaPart`. The loop and provider adapter decide how to serialize the output when building the next model request.

## Typed identifiers

agentkit uses newtype wrappers for all identifiers:

```rust
pub struct SessionId(pub String);
pub struct TurnId(pub String);
pub struct MessageId(pub String);
pub struct ToolCallId(pub String);
pub struct TaskId(pub String);
pub struct ApprovalId(pub String);
pub struct ProviderMessageId(pub String);
pub struct ArtifactId(pub String);
pub struct PartId(pub String);
```

All generated by the same `id_newtype!` macro, which derives `Clone`, `Debug`, `Serialize`, `Deserialize`, `Display`, `Hash`, `Eq`, `Ord`, and conversions from `&str` and `String`.

This prevents accidental mix-ups — passing a `ToolCallId` where a `TaskId` is expected is a compile error, not a runtime bug. The cost is some verbosity when constructing IDs (`SessionId::new("my-session")` instead of `"my-session"`), but the safety benefit compounds across a codebase where dozens of string IDs flow through multiple layers.

```text
Without newtypes:

  fn execute(call_id: String, task_id: String, session_id: String) { ... }
  execute(session_id, call_id, task_id);  // compiles, wrong at runtime

With newtypes:

  fn execute(call_id: ToolCallId, task_id: TaskId, session_id: SessionId) { ... }
  execute(session_id, call_id, task_id);  // compile error
```

## The metadata bag

Every significant type carries a `MetadataMap`:

```rust
pub type MetadataMap = BTreeMap<String, serde_json::Value>;
```

This is the extension point. Provider-specific data (like an OpenAI `logprobs` field or an OpenRouter `cost` value) lives in namespaced metadata keys rather than polluting the core schema. The convention is `provider_name.field_name`:

| Metadata key                | Source             | Example value                   |
| --------------------------- | ------------------ | ------------------------------- |
| `openrouter.model`          | OpenRouter adapter | `"anthropic/claude-3.5-sonnet"` |
| `openrouter.refusal`        | OpenRouter adapter | `"I cannot help with that"`     |
| `agentkit.interrupted`      | Loop driver        | `true`                          |
| `agentkit.interrupt_reason` | Loop driver        | `"user_cancelled"`              |

`BTreeMap` is used instead of `HashMap` for deterministic serialization order — metadata roundtrips through JSON identically regardless of insertion order. This matters for snapshot testing and transcript persistence.

The rest of the stack never depends on metadata for correctness. It's there for observability, debugging, and host-specific extensions.

## Usage and finish reasons

Token counts and costs are first-class:

```rust
pub struct Usage {
    pub tokens: Option<TokenUsage>,
    pub cost: Option<CostUsage>,
    pub metadata: MetadataMap,
}

pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
}

pub struct CostUsage {
    pub amount: f64,
    pub currency: String, // ISO 4217, e.g. "USD"
    pub provider_amount: Option<String>,
}
```

Not all providers report all fields. `TokenUsage` uses `Option` for fields that only some providers support (reasoning tokens, cached input tokens, cache write tokens). The `Usage` struct itself wraps both token and cost in `Option` because some providers report one without the other.

Finish reasons are normalized to a small, stable enum:

```rust
pub enum FinishReason {
    Completed,   // Normal completion
    ToolCall,    // Stopped to invoke tools
    MaxTokens,   // Hit the token limit
    Cancelled,   // User-initiated cancellation
    Blocked,     // Content policy violation
    Error,       // Generation error
    Other(String),
}
```

The loop inspects `FinishReason` to decide what to do next:

| `FinishReason` | Loop behaviour                                   |
| -------------- | ------------------------------------------------ |
| `Completed`    | Return `TurnResult` to the host                  |
| `ToolCall`     | Execute tools, start another model turn          |
| `MaxTokens`    | Return `TurnResult` (host may submit more input) |
| `Cancelled`    | Return `TurnResult` with cancellation metadata   |
| `Blocked`      | Return `TurnResult` (host may adjust the prompt) |
| `Error`        | Return error to the host                         |
| `Other(s)`     | Treat as `Completed` (log the unknown reason)    |

Providers map their native stop reasons into this enum. The original value can be preserved in metadata if needed.

## Cancellation primitives

agentkit supports cooperative turn cancellation through a generation-counter pattern:

```rust
pub struct CancellationController { /* Arc<AtomicU64> */ }
pub struct CancellationHandle { /* Arc<AtomicU64> */ }
pub struct TurnCancellation { handle: CancellationHandle, generation: u64 }
```

The three types form a publish-subscribe pattern:

```text
CancellationController              CancellationHandle
(owned by the host)                 (shared with loop + tools)
        │                                   │
        │  interrupt()                      │  checkpoint()
        │  ─────────▶ bumps AtomicU64       │  ────────────▶ TurnCancellation
        │             (generation: 0→1)     │                 { generation: 0 }
        │                                   │
        │                                   │  After interrupt():
        │                                   │  checkpoint.is_cancelled() → true
        │                                   │  (because 0 ≠ 1)
```

The controller increments a counter. Any `TurnCancellation` checkpoint created before the increment reports itself as cancelled. This is lightweight (one `AtomicU64`), lock-free, and works in `tokio::select!` to race a model call against user interruption:

```rust
tokio::select! {
    result = model_turn.next_event(None) => { /* process event */ }
    _ = cancellation.cancelled() => { /* turn was cancelled */ }
}
```

The `cancelled()` method polls every 10ms — fast enough for responsive cancellation, cheap enough to run alongside every model call.

## Per-item bookkeeping fields

`Item` carries a few optional fields that the loop populates as the transcript is built. They are not part of the wire format — they're host-side bookkeeping that downstream consumers (compaction triggers, reporters, persistence) can read without rebuilding their own indexes:

```rust
pub struct Item {
    pub id: Option<MessageId>,
    pub kind: ItemKind,
    pub parts: Vec<Part>,
    pub metadata: MetadataMap,
    pub usage: Option<Usage>,           // populated by the loop on assistant items
    pub finish_reason: Option<FinishReason>, // populated by the loop on assistant items
    pub created_at: Option<Timestamp>,  // stamped by the loop on every appended item
}
```

`Timestamp(u64)` is wall-clock milliseconds since the Unix epoch. The loop stamps `created_at` exactly once per item, when it lands in the transcript. Consumers can sort, filter, or expire by age without depending on a particular date-time crate.

This is what lets `context_window_trigger` find "the last reported `input_tokens`" by walking the transcript directly, instead of needing a separate `Usage` channel to listen on.

## Design principles

Three principles guide the core data model:

1. **Normalize what the rest of the stack must reason about.** If the loop, tools, compaction, and reporting all need to understand something, it gets a first-class type in core. This is why `FinishReason` has explicit variants for `ToolCall` and `Cancelled` rather than encoding them as metadata — the loop's branching logic depends on them.

2. **Don't force provider wire formats into the public API.** Providers keep their native types internally. They project into core types at the boundary. A provider that uses `"stop"` for `Completed` and `"end_turn"` for `ToolCall` handles the mapping in its adapter — the loop never sees provider-native strings.

3. **Preserve provider-specific data without polluting the model.** A small number of first-class fields (parts, usage, finish reason) plus an open-ended `MetadataMap` on every type. The first-class fields cover what the framework must understand; metadata covers what specific integrations care about.

## Error types

`agentkit-core` defines three error types used across the workspace:

- `NormalizeError` — content cannot be projected into the agentkit data model (e.g. an unsupported media type)
- `ProtocolError` — the provider or loop reached an invalid state (e.g. a tool result without a matching call)
- `AgentError` — unifies both via `From` impls, used as the top-level error type

These are intentionally minimal. Each downstream crate defines its own error types (like `LoopError`, `ToolError`, `CompactionError`) that wrap or convert from these when needed.

> **Crate:** [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core) — this entire chapter describes types defined in this single crate. It has no runtime dependencies and no async code. Every other crate in the workspace depends on it.
