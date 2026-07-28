# The capability layer

Before we discuss tools, we need to understand the abstraction they build on. `agentkit-capabilities` defines a lower-level interoperability layer for anything a model can interact with: operations it can invoke, data it can read, and prompt templates it can use.

## Why a layer beneath tools

The current design has three external capability shapes:

- **Invocables** — named request/response operations (tools, MCP tools, custom operations)
- **Resources** — named data blobs that can be listed and read (files, database rows, API responses)
- **Prompts** — parameterized templates that produce conversation items

Native tools and MCP tools are both invocable operations. But MCP also exposes resources and prompts, which are not tools. Forcing everything through a `Tool` trait would distort the model — reading a resource is not a tool call, and rendering a prompt template is not tool execution.

```text
Without a capability layer:

  Tool trait ◀── native tools
             ◀── MCP tools (fit naturally)
             ◀── MCP resources (forced into tool shape — read_resource "tool")
             ◀── MCP prompts (forced into tool shape — render_prompt "tool")

  Everything is a tool. But reading a resource has no side effects,
  no permission model, and no schema. Wrapping it as a "tool" adds
  complexity without adding value.


With a capability layer:

  Invocable  ◀── native tools (via Tool → Invocable bridge)
             ◀── MCP tools (via McpToolAdapter)

  ResourceProvider ◀── MCP resources
                   ◀── custom data sources

  PromptProvider   ◀── MCP prompts
                   ◀── custom template engines

  Each shape gets the right abstraction. No forced fitting.
```

The capability layer gives MCP, tools, and future integrations one shared vocabulary without pretending everything is the same thing.

## Invocable

The core trait for anything the model can call:

```rust
#[async_trait]
pub trait Invocable: Send + Sync {
    fn spec(&self) -> &InvocableSpec;
    async fn invoke(
        &self,
        request: InvocableRequest,
        ctx: &mut CapabilityContext<'_>,
    ) -> Result<InvocableResult, CapabilityError>;
}
```

An `InvocableSpec` carries the name, description, and JSON Schema for the input — enough information to present the capability to a model:

```rust
pub struct InvocableSpec {
    pub name: CapabilityName,
    pub description: String,
    pub input_schema: Value,       // JSON Schema object
    pub metadata: MetadataMap,
}
```

The request carries the model's input arguments plus session context:

```rust
pub struct InvocableRequest {
    pub input: Value,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub metadata: MetadataMap,
}
```

And the result supports multiple return shapes:

```rust
pub struct InvocableResult {
    pub output: InvocableOutput,
    pub metadata: MetadataMap,
}

pub enum InvocableOutput {
    Text(String),             // Plain text response
    Structured(Value),        // JSON value
    Items(Vec<Item>),         // Conversation items (for prompts, multi-part results)
    Data(DataRef),            // Binary or referenced data
}
```

### Invocable vs Tool

`Invocable` is deliberately thinner than `Tool`:

| `Invocable`                          | `Tool`                                               |
| ------------------------------------ | ---------------------------------------------------- |
| `spec: InvocableSpec`                | `spec: ToolSpec` (adds annotations)                  |
| `invoke(request, CapabilityContext)` | `invoke(request, ToolContext)`                       |
|                                      | `proposed_requests()` (preflight)                    |
|                                      | `ToolAnnotations` (read_only, destructive, ...)      |
|                                      | `ToolContext` (permissions, resources, cancellation) |

An `Invocable` knows its name, description, schema, and how to execute. A `Tool` adds permission semantics, behavioural hints, and a richer execution context. Tools are invocables with opinions about safety.

## Resources and prompts

```rust
#[async_trait]
pub trait ResourceProvider: Send + Sync {
    async fn list_resources(&self) -> Result<Vec<ResourceDescriptor>, CapabilityError>;
    async fn read_resource(&self, id: &ResourceId, ctx: &mut CapabilityContext<'_>)
        -> Result<ResourceContents, CapabilityError>;
}
```

Resources are named data blobs. They have an ID, a name, an optional description and MIME type. Reading them returns a `DataRef` — the content might be inline text, inline bytes, or a URI:

```rust
pub struct ResourceDescriptor {
    pub id: ResourceId,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub metadata: MetadataMap,
}

pub struct ResourceContents {
    pub data: DataRef,
    pub metadata: MetadataMap,
}
```

Prompts are parameterized templates that produce conversation items:

```rust
#[async_trait]
pub trait PromptProvider: Send + Sync {
    async fn list_prompts(&self) -> Result<Vec<PromptDescriptor>, CapabilityError>;
    async fn get_prompt(&self, id: &PromptId, args: Value, ctx: &mut CapabilityContext<'_>)
        -> Result<PromptContents, CapabilityError>;
}
```

A prompt descriptor carries a JSON Schema for its arguments:

```rust
pub struct PromptDescriptor {
    pub id: PromptId,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub metadata: MetadataMap,
}

pub struct PromptContents {
    pub items: Vec<Item>, // Rendered conversation items
    pub metadata: MetadataMap,
}
```

These are separate traits, not specializations of `Invocable`. The type system enforces the distinction — you can't accidentally pass a `ResourceProvider` where an `Invocable` is expected.

| Capability type    | Model interaction     | Side effects | Permission model      |
| ------------------ | --------------------- | ------------ | --------------------- |
| `Invocable`        | Model calls it        | May have     | Full tool permissions |
| `ResourceProvider` | Host reads, injects   | Read-only    | Simpler (list + read) |
| `PromptProvider`   | Host renders, injects | None         | None (templates only) |

## CapabilityProvider

Many integrations expose multiple capability kinds. The `CapabilityProvider` trait bundles them:

```rust
pub trait CapabilityProvider: Send + Sync {
    fn invocables(&self) -> Vec<Arc<dyn Invocable>>;
    fn resources(&self) -> Vec<Arc<dyn ResourceProvider>>;
    fn prompts(&self) -> Vec<Arc<dyn PromptProvider>>;
}
```

An MCP server implements `CapabilityProvider` to expose its tools, resources, and prompts through one registration point:

```text
MCP server "github"
  │
  ├── invocables:  [search_issues, create_pr, merge_pr]
  ├── resources:   [repo_readme, issue_list, pr_diff]
  └── prompts:     [code_review_prompt, bug_report_template]
       │
       ▼
  CapabilityProvider::invocables()  → Vec<Arc<dyn Invocable>>
  CapabilityProvider::resources()   → Vec<Arc<dyn ResourceProvider>>
  CapabilityProvider::prompts()     → Vec<Arc<dyn PromptProvider>>
```

The loop collects all capability providers and merges their invocables into the unified tool list presented to the model. Resources and prompts flow through separate paths — they're typically consumed by the context loader or the host, not directly by the model.

## CapabilityContext

```rust
pub struct CapabilityContext<'a> {
    pub session_id: Option<&'a SessionId>,
    pub turn_id: Option<&'a TurnId>,
    pub metadata: &'a MetadataMap,
}
```

This is a minimal context passed to all capability invocations. It carries enough to correlate work with a session and turn, but not enough to reach into the loop or modify the transcript.

The tool layer wraps this in a richer `ToolContext` that adds permission checking, shared resources, and cancellation:

| `CapabilityContext` (lean) | `ToolContext` (rich)                     |
| -------------------------- | ---------------------------------------- |
| `session_id`               | `capability: CapabilityContext`          |
| `turn_id`                  | `permissions: &dyn PermissionChecker`    |
| `metadata`                 | `resources: &dyn ToolResources`          |
|                            | `cancellation: Option<TurnCancellation>` |

The capability layer doesn't know about permissions or cancellation. These are tool-layer concerns, added by the `ToolContext` wrapper.

## Error handling

All capability traits use a single error type:

```rust
pub enum CapabilityError {
    Unavailable(String),     // Capability not found or offline
    InvalidInput(String),    // Arguments failed validation
    ExecutionFailed(String), // Runtime failure
}
```

This is intentionally coarse-grained. The capability layer doesn't try to enumerate every failure mode — it provides three buckets that cover the meaningful distinctions: "doesn't exist", "bad input", and "broken at runtime". Downstream layers (tools, MCP) add their own error types when finer granularity is needed.

## Positioning

This layer is public and extensible, but it is not the primary extension point for most users. The intended guidance:

| "I want to..."                            | Implement...                                 |
| ----------------------------------------- | -------------------------------------------- |
| Add a custom tool that the model can call | `Tool` trait ([ch09](./ch09-tool-system.md)) |
| Expose data for context loading           | `ResourceProvider`                           |
| Expose parameterized prompt templates     | `PromptProvider`                             |
| Integrate an MCP server                   | `CapabilityProvider` ([ch17](./ch17-mcp.md)) |
| Build something that doesn't fit above    | `Invocable` directly                         |

Most users implement `Tool`. The capability traits matter when you're integrating MCP servers, building custom data sources, or working on the framework itself.

> **Crate:** [`agentkit-capabilities`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-capabilities) — depends only on [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core). No runtime dependencies, no async runtime requirements beyond the traits themselves.
