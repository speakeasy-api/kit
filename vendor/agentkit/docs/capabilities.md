# agentkit-capabilities design

## Purpose

`agentkit-capabilities` is the lower-level interoperability layer beneath tools and MCP.

It exists because the current design has three distinct external capability shapes:

- invocable operations
- readable resources
- parameterized prompts

Native tools and MCP tools are both invocable operations. MCP also exposes resources and prompts, which are not tools.

This crate should define the shared capability model that:

- `agentkit-tools-core` can build on top of
- `agentkit-mcp` can implement comprehensively
- future integrations can adopt without pretending everything is a tool

This layer should be public and extensible in v1.

But it should not be the primary extension path for most users.

Recommended positioning:

- custom tools are the default extension mechanism
- custom capabilities are the advanced escape hatch when the tool abstraction is not sufficient

## Non-goals

`agentkit-capabilities` should not own:

- the model-facing tool abstraction
- permission policy
- the loop driver
- MCP transport/session management
- context loading policy

It is a generic capability model, not the orchestration or policy layer.

## Design principles

### 1. Separate invocables, resources, and prompts

These are different shapes and should stay different.

Trying to collapse them into one trait would make the abstraction less useful.

### 2. Keep the layer lower than tools

The model-facing “tool” abstraction is a specialization:

- tools are invocables
- tools add model-facing schema and permission semantics

That means tools should build on this layer, not define it.

### 3. Keep the layer lower than MCP

MCP is one producer of capabilities, not the definition of capability itself.

That means:

- MCP tools map to invocables
- MCP resources map to resources
- MCP prompts map to prompts

## Core concepts

## 1. Invocable

An invocable is a named request/response operation.

Recommended shape:

```rust
pub struct InvocableSpec {
    pub name: CapabilityName,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub metadata: MetadataMap,
}

pub trait Invocable: Send + Sync {
    fn spec(&self) -> &InvocableSpec;

    async fn invoke(
        &self,
        request: InvocableRequest,
        ctx: &mut CapabilityContext,
    ) -> Result<InvocableResult, CapabilityError>;
}
```

This is the shared base for:

- native tools
- MCP tools
- future remote operation providers

## 2. Resource provider

Resources are addressable artifacts that can be listed and read.

Recommended shape:

```rust
pub trait ResourceProvider: Send + Sync {
    async fn list_resources(&self) -> Result<Vec<ResourceDescriptor>, CapabilityError>;
    async fn read_resource(
        &self,
        id: &ResourceId,
        ctx: &mut CapabilityContext,
    ) -> Result<ResourceContents, CapabilityError>;
}
```

## 3. Prompt provider

Prompts are parameterized prompt-construction helpers.

Recommended shape:

```rust
pub trait PromptProvider: Send + Sync {
    async fn list_prompts(&self) -> Result<Vec<PromptDescriptor>, CapabilityError>;
    async fn get_prompt(
        &self,
        id: &PromptId,
        args: serde_json::Value,
        ctx: &mut CapabilityContext,
    ) -> Result<PromptContents, CapabilityError>;
}
```

## 4. Capability provider

Many integrations expose more than one capability kind.

Recommended aggregate interface:

```rust
pub trait CapabilityProvider: Send + Sync {
    fn invocables(&self) -> Vec<Arc<dyn Invocable>>;
    fn resources(&self) -> Vec<Arc<dyn ResourceProvider>>;
    fn prompts(&self) -> Vec<Arc<dyn PromptProvider>>;
}
```

This is the right place for MCP-like integrations to plug in.

## 5. Capability context

The lower-level layer needs a small execution context that works both inside and outside the loop.

Recommended shape:

```rust
pub struct CapabilityContext<'a> {
    pub session_id: Option<&'a SessionId>,
    pub turn_id: Option<&'a TurnId>,
    pub metadata: &'a MetadataMap,
}
```

`agentkit-tools-core` can wrap this in a richer tool execution context later.

## Relationship to tools

`agentkit-tools-core` should specialize this layer:

- `ToolSpec` can wrap or derive from `InvocableSpec`
- `Tool` can extend `Invocable` with preflight action and permission semantics
- the tool executor can apply policy around an invocable-backed operation

This keeps tool-specific concerns out of the lowest common layer.

This should also drive how the library is presented to users:

- "implement a custom tool" should be the first recommendation
- "implement a custom capability provider" should be reserved for integrations that expose resources, prompts, or non-tool invocables

## Relationship to MCP

`agentkit-mcp` should implement this layer:

- MCP tools as invocables
- MCP resources as resource providers
- MCP prompts as prompt providers

This gives MCP one coherent integration point without forcing everything through `Tool`.

Because MCP implements the capability layer directly, it should also own the bridge into context-oriented usage for MCP resources and prompts.

## Relationship to loop

The loop still mostly cares about tools because model tool-calling is tool-shaped.

But this lower layer helps because:

- native tools and MCP tools can share the same invocable base
- context integrations can consume resources/prompts without going through the tool path

So the loop does not have to depend heavily on `agentkit-capabilities`, but the workspace architecture should.

## Proposed workspace impact

The workspace should add:

```text
crates/
  agentkit-core
  agentkit-capabilities
  agentkit-loop
  agentkit-tools-core
  agentkit-mcp
  ...
```

Recommended dependency direction:

- `agentkit-core`
- `agentkit-capabilities` depends on `agentkit-core`
- `agentkit-tools-core` depends on `agentkit-capabilities`
- `agentkit-mcp` depends on `agentkit-capabilities`
- `agentkit-loop` depends on `agentkit-tools-core`

## What we should validate early

Before locking this layer, prove:

1. a native tool can be implemented cleanly as an invocable-backed tool
2. an MCP server can expose tools/resources/prompts through one capability provider
3. the abstraction does not force resources/prompts through the tool path
4. the extra layer does not add painful boilerplate for simple tools

If any of those fail, this layer is either too abstract or in the wrong place.
