# agentkit-core design

## Purpose

`agentkit-core` is the contract crate for the rest of the workspace.

It should define the smallest stable set of types and traits that every higher-level crate can rely on:

- normalized content and message shapes
- identifiers and envelope types
- tool call and tool result value types
- usage and finish metadata
- provider projection traits
- shared error types

If a type must be understood by both the loop and multiple extension crates, it probably belongs here.
If a type controls execution, IO, runtime behavior, or policy, it probably does not.

## Non-goals

`agentkit-core` should not own:

- async runtimes
- the loop driver
- approval flow
- reporter interfaces
- tool execution traits
- shell or filesystem behavior
- MCP transport or process management
- compaction policy

This crate defines data and views, not operational behavior.

## Dependency policy

`agentkit-core` should stay runtime-agnostic.

Design constraints:

- no `tokio`
- no process APIs
- no filesystem APIs
- no networking APIs
- no async trait requirements

Reasonable dependencies:

- `serde` and `serde_json`
- `thiserror`
- small utility crates for IDs or timestamps if needed

Optional features are fine, but the base crate should remain lightweight enough for embedded or constrained environments.

## Design principles

### 1. Normalize what the rest of the stack must reason about

The loop, tools, compaction, and reporting need a shared understanding of:

- text and multimodal content
- tool calls
- tool results
- usage
- finish reasons

Those concepts should be first-class core types.

The model must also scale cleanly across provider shapes:

- text-only providers
- voice-only providers
- fully multimodal providers

The goal is that complexity grows roughly linearly with the number of modalities a provider exposes, not combinatorially with every possible provider combination.

### 2. Do not force provider wire formats into the public API

Providers should be free to keep their native request and response types.

But they must be able to project those types into `agentkit-core` views so the rest of the toolkit can operate without provider-specific branching.

### 3. Preserve provider-specific metadata without polluting the model

The core model should have a small number of first-class fields and an extension bag for opaque provider metadata.

That means:

- normalized fields for things the toolkit actually uses
- namespaced metadata for provider-specific extras

### 4. Prefer additive, composable types

The core should be easy to extend without breaking downstream crates.

That argues for:

- explicit enums for stable categories
- metadata bags for unstable provider details
- newtype IDs instead of raw strings where identity matters

## What belongs in core

## 1. Identifiers

The toolkit will need stable identities across turns and subsystems.

Recommended newtypes:

- `SessionId`
- `TurnId`
- `MessageId`
- `ToolCallId`
- `ToolResultId`
- `ProviderMessageId`

These should be plain data wrappers with display and serialization support.

They matter because reporting, transcript persistence, compaction, and approval flows all need to refer to the same objects safely.

## 2. Roles and items

The core model should be item-based rather than tied to old chat-message assumptions.

Recommended shape:

```rust
pub struct Item {
    pub id: Option<MessageId>,
    pub kind: ItemKind,
    pub parts: Vec<Part>,
    pub metadata: MetadataMap,
    /// Token / cost usage for the turn that produced this item.
    /// Populated by the loop on assistant items.
    pub usage: Option<Usage>,
    /// Why the turn that produced this item ended.
    /// Populated by the loop on assistant items.
    pub finish_reason: Option<FinishReason>,
    /// When this item was appended to the transcript (ms since epoch).
    /// Stamped by the loop on every appended item.
    pub created_at: Option<Timestamp>,
}

pub enum ItemKind {
    System,
    Developer,
    User,
    Assistant,
    Tool,
    Context,
}
```

Why item-based:

- modern providers are converging on content blocks rather than one raw text field
- tools and multimodal content fit more naturally
- compaction can summarize at the item level
- it gives a single durable envelope that can represent text-only, voice-only, and multimodal turns with the same structure

`Context` is worth keeping distinct from `System` and `Developer` because loaded project instructions and skills are not always the same thing as hardcoded product prompts.

## 3. Content model

The content model should be provider-neutral, inspired by content-block-style APIs, but not coupled to any provider SDK or response schema.

More importantly, it should be resilient to provider differences.

That means:

- the durable transcript format should always be `Item { parts: Vec<Part> }`
- each part should represent one coherent unit of content
- provider-specific differences should be allowed to survive through metadata and extension parts
- the model should not become exponentially more complex as more modalities are supported

V1 should ship with a comprehensive multimodal part set.

Reason:

- `agentkit` should feel like a compelling alternative to existing agent SDKs
- users should not need to immediately fall back to `Custom` for common modalities
- text-only, voice-only, and multimodal providers should all map naturally on day one

Recommended v1 shape:

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

Recommended part semantics:

- `Text`: plain user-visible text
- `Media`: modality-tagged media such as audio, image, or video
- `File`: file reference or uploaded artifact descriptor
- `Structured`: typed structured payload that should remain machine-readable
- `Reasoning`: provider-supplied reasoning summary or reasoning block when available
- `ToolCall`: a request by the model to invoke a named tool with structured input
- `ToolResult`: structured output returned from a tool
- `Custom`: escape hatch for provider-specific part types that do not map cleanly yet

The key rules are:

- `ToolCall` and `ToolResult` are content parts, not side channels
- multimodal content is first-class, not hidden in metadata
- provider-specific differences may leak across boundaries through `Custom` and namespaced metadata without breaking the rest of the model

Recommended supporting shapes:

```rust
pub struct MediaPart {
    pub modality: Modality,
    pub mime_type: String,
    pub data: DataRef,
    pub metadata: MetadataMap,
}

pub enum Modality {
    Audio,
    Image,
    Video,
    Binary,
}

pub enum DataRef {
    InlineText(String),
    InlineBytes(Vec<u8>),
    Uri(String),
    Handle(ArtifactId),
}
```

This is what makes a voice-only provider and a text-only provider equally natural to represent:

- text-only provider: `Item` with `Text` parts
- voice-only provider: `Item` with `Media { modality: Audio, ... }`
- multimodal provider: `Item` with multiple parts

That gives the desired linear complexity increase as modalities are added.

To make this comprehensive in practice, the supporting part types should cover the common cases directly rather than forcing everything through opaque blobs.

Recommended v1 part coverage:

- `TextPart`
  - plain text
  - optional formatting or segmentation metadata
- `MediaPart`
  - audio input/output
  - image input/output
  - video references
  - generic binary media fallback
- `FilePart`
  - uploaded files
  - generated artifacts
  - local or remote file references
- `StructuredPart`
  - arbitrary JSON payloads
  - typed structured model outputs
  - schema-carrying machine-readable data
- `ReasoningPart`
  - reasoning summaries
  - redacted/private reasoning references where available
- `ToolCallPart`
  - model-issued tool invocation requests
- `ToolResultPart`
  - normalized tool results
- `CustomPart`
  - provider-specific escape hatch for genuinely unmapped content

The intended v1 standard is:

- common modalities should map to first-class parts
- `CustomPart` should be rare, not the normal path

## 4. Text and streaming deltas

The core should distinguish between durable content and streamed deltas.

Recommended split:

- `Part` for completed transcript content
- `Delta` for streamed output fragments and part construction events

Recommended v1 shape:

```rust
pub enum Delta {
    BeginPart { part_id: PartId, kind: PartKind },
    AppendText { part_id: PartId, chunk: String },
    AppendBytes { part_id: PartId, chunk: Vec<u8> },
    ReplaceStructured {
        part_id: PartId,
        value: serde_json::Value,
    },
    SetMetadata { part_id: PartId, metadata: MetadataMap },
    CommitPart { part: Part },
}
```

This model is more resilient than making deltas mirror final parts one-for-one.

It reflects what usually happens in practice:

- during the turn, providers stream partial updates
- the loop or adapter folds those updates into finalized parts
- after the turn, the durable transcript is stored as `Item` values with committed parts
- on the next turn, transcript items are reused, not raw deltas

So the right rule is:

- deltas are transient and incremental
- parts are durable and replayable

`Delta` belongs in core because adapters, the loop, and reporters all need to agree on what a streamed update means and how it folds into a final transcript shape.

For v1, `Delta` should also be comprehensive enough for common streaming behaviors:

- incremental text streaming
- incremental audio or binary chunk streaming when a provider supports it
- staged tool call construction if needed
- final structured payload replacement or commit
- metadata updates during the turn

Recommended v1 delta coverage:

- `BeginPart`
- `AppendText`
- `AppendBytes`
- `ReplaceStructured`
- `SetMetadata`
- `CommitPart`

This keeps the delta model generic without forcing a bespoke delta variant for every final part type.

## 5. Tool value types

Even if tool execution traits live in `agentkit-tools-core`, the value types for tool interaction should live in core.

Recommended shape:

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

Recommended `ToolOutput` shape:

- text
- structured JSON
- file references
- mixed content parts

This is important because different providers want tool results serialized differently, but `agentkit` should retain a richer normalized form before adapter-specific re-encoding.

## 6. Usage and finish metadata

These belong in core because the loop, reporters, and cost accounting all depend on them.

Recommended types:

- `Usage`
- `TokenUsage`
- `CostUsage`
- `FinishReason`

`FinishReason` should be normalized to a small stable set:

- `Completed`
- `ToolCall`
- `MaxTokens`
- `Cancelled`
- `Blocked`
- `Error`
- `Other(String)`

Providers can map their native stop reasons into this enum and keep the original value in metadata if needed.

## 7. Metadata bag

Core types should consistently support opaque extension metadata.

Recommended shape:

```rust
pub type MetadataMap = BTreeMap<String, serde_json::Value>;
```

Recommended convention:

- provider-specific keys should be namespaced, for example `openai.*` or `anthropic.*`
- `agentkit` crates should not depend on provider-specific metadata for correctness

This is the cleanest way to preserve useful details without making them core semantics.

## 8. Projection traits

This is the most important part of the design.

You suggested that providers should be able to encode their own message types and expose traits for extracting what `agentkit` needs. I think that is right, and `agentkit-core` is where those traits should live.

The goal is:

- provider adapters keep native types internally
- loop and reporters consume normalized views
- conversion stays explicit and testable
- provider-specific data can survive normalization through metadata or custom parts when needed

There are two viable patterns.

### Pattern A: full normalization

The adapter converts provider-native data into concrete `agentkit-core` types immediately.

Example:

```rust
fn normalize_message(native: ProviderMessage) -> Item
```

Pros:

- simple for downstream crates
- easy to serialize, store, and test

Cons:

- may allocate more
- may lose some structure unless metadata is preserved carefully

### Pattern B: borrowed projection traits

The adapter keeps native types and exposes accessor methods that return normalized borrows.

Pros:

- keeps provider ownership and layout flexible
- can reduce copying

Cons:

- more lifetime complexity
- harder to store and pass around dynamically

## Recommendation

v1 lands on Pattern A across the workspace: the canonical normalized model is the concrete `Item` struct, and every adapter materialises owned values at its boundaries.

- `agentkit-core` defines concrete normalized types as the canonical interchange model
- `agentkit-loop` normalises to owned core types at its boundaries
- adapters that want zero-copy internals keep that inside the adapter and convert on egress

That gives efficient adapters a path to stay zero-copy internally, while keeping the rest of the system simple.

The normalized model should therefore be:

- open enough to represent cross-provider differences without pain
- constrained enough that the rest of the stack can build stable behavior on top of it

That is the reason for the `Item` plus `Part` plus `Delta` approach.

For v1, the design target should be:

- comprehensive first-class support for common multimodal content
- `CustomPart` reserved for genuinely unusual provider-specific cases
- enough delta expressiveness to support both classic SSE text streaming and richer multimodal streams

## 9. Shared errors

The core error model should be small and composable.

Recommended types:

- `AgentError`: top-level shared error enum
- `NormalizeError`: conversion/projection failures
- `ProtocolError`: invalid provider or tool interaction states

Avoid turning `AgentError` into a kitchen sink. Domain-specific crates should wrap or convert into it only where cross-crate handling matters.

## What does not belong in core

These should live elsewhere:

- `ModelAdapter` and `ModelSession`: `agentkit-loop` or a closely related crate, because they are operational interfaces
- `Reporter`: `agentkit-reporting`
- `Tool` execution traits and registry: `agentkit-tools-core`
- `ApprovalRequest`: `agentkit-loop`
- permission policies: `agentkit-tools-core` or a dedicated policy crate later

The core crate should not become a dumping ground for “shared” concepts that are only shared because nothing else has been designed yet.

## Proposed module layout

```text
agentkit-core/
  src/
    lib.rs
    ids.rs
    item.rs
    content.rs
    delta.rs
    tool.rs
    usage.rs
    finish.rs
    metadata.rs
    error.rs
```

Module intent:

- `ids.rs`: newtype identifiers
- `item.rs`: `Item` and `ItemKind`
- `content.rs`: durable `Part` types and multimodal data references
- `delta.rs`: streaming `Delta` types and foldable part construction events
- `tool.rs`: tool call/result value types
- `usage.rs`: tokens, cost, usage snapshots
- `finish.rs`: normalized finish reasons
- `metadata.rs`: metadata bag and conventions
- `error.rs`: shared errors

## API stability bar

The `agentkit-core` bar should be higher than other crates because churn here spreads everywhere.

Before stabilizing the first public version, we should prove:

1. one fake stateless provider can map into these types cleanly
2. one fake stateful provider can map into these types cleanly
3. shell and filesystem tool results fit without awkward escaping
4. reporters can stream deltas without provider-specific branching
5. compaction can summarize `Item`s without needing hidden provider data

If any of those fail, the core model is missing something important.

## Recommended first implementation scope

The first pass of `agentkit-core` should be intentionally small.

Implement first:

- IDs
- `Item` and `ItemKind`
- `Part`
- `Delta`
- `ToolCallPart`
- `ToolResultPart`
- `Usage`
- `FinishReason`
- `MetadataMap`
- projection traits
- shared errors

Do not implement first:

- deeply nested multimodal special cases
- elaborate serialization formats
- approval concepts
- runtime traits

But do implement in the first public release:

- a comprehensive multimodal `Part` set
- a comprehensive but generic `Delta` set
- examples proving text-only, voice-only, and multimodal provider mappings

The fastest way to validate core is to use it immediately from a fake provider and a fake reporter. If those two consumers feel natural, the crate boundary is probably right.
