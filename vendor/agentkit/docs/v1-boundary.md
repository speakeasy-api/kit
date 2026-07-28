# agentkit v1 boundary

## Product statement

`agentkit` is a Rust toolkit for building LLM-powered agent applications.

It should make it possible to assemble a coding-agent-style app from reusable parts:

- an agent loop
- a model adapter implemented by the app author
- a tool registry
- permission controls
- context loaders
- compaction hooks
- structured reporting

The library should help users build their own product. It should not try to be the product.

## What v1 should optimize for

1. Building terminal and backend agents with low ceremony.
2. Supporting both stateless and stateful model providers behind one loop abstraction.
3. Making tools easy to write and safe to constrain.
4. Keeping storage, provider choice, and UI policy outside the framework.
5. Keeping features modular enough that users can depend on only the parts they need.

## What v1 is not

These are explicit non-goals for the first version:

- No built-in provider integrations for OpenAI, Anthropic, OpenRouter, Ollama, or others.
- No built-in UI, TUI, web app, or CLI shell product.
- No hardcoded planning style, prompt stack, or "best" agent persona.
- No persistent memory store, vector DB integration, or hosted sync service.
- No sandbox implementation. `agentkit` should expose policy hooks and tool constraints, but not pretend to be a VM or container manager.
- No browser automation stack in v1 unless there is a concrete consumer driving it.

That line matters. Otherwise `agentkit` turns into "an opinionated coding agent with some extension points", which is a different product.

## The main architectural boundary

The key split should be:

- `agentkit` owns orchestration primitives.
- The application owns policy and product decisions.

In practice that means:

- `agentkit` defines the loop, event model, tool traits, permission hooks, compaction hooks, and reporting traits.
- The application decides which provider adapter to use, which tools to expose, when to require approval, where to load context from, how to store transcripts, and how to present output.

## Core concepts

### 1. Model adapter

The model abstraction should unify stateless and stateful providers.

Recommended shape:

```rust
pub trait ModelAdapter {
    type Session: ModelSession;

    async fn start_session(
        &self,
        config: SessionConfig,
    ) -> Result<Self::Session, AgentError>;
}

pub trait ModelSession {
    async fn run_turn(
        &mut self,
        input: TurnInput,
    ) -> Result<TurnStream, AgentError>;
}
```

The important design choice is that the loop talks to a `ModelSession`, not directly to a stateless request function.

- A stateless provider can implement `ModelSession` by rebuilding full history on every turn.
- A stateful provider can implement `ModelSession` by keeping a remote session ID, websocket, or other provider-native state.

This keeps the loop stable while allowing radically different backends.

The second design choice is that the provider may keep its own native message types internally, but `agentkit` still needs a canonical view for orchestration.

Recommended rule:

- adapters may use provider-specific request and response types
- those types should project into shared `agentkit` traits and normalized event/content shapes
- the loop should never need to understand provider-native wire formats directly

That gives flexibility without letting provider-specific types leak into every crate.

### 2. Agent loop

The loop should do one job well: run a turn, stream model events, execute requested tools, feed results back, and emit structured events.

The public control surface should be loop-like, not just "call one async method and get everything at the end".

The important distinction is:

- blocking items are returned by the driver and must be answered by the host before progress continues
- non-blocking items are reported through pluggable reporters or event sinks

The loop should only stop on true interrupts, not on ordinary reporting activity.

Recommended shape:

```rust
let mut driver = agent.driver(session);

loop {
    match driver.next().await? {
        LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(request)) => {
            /* ask host/user, then resume */
        }
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(state)) => {
            /* collect next user turn, then resume */
        }
        LoopStep::Finished(turn) => break,
    }
}
```

That design is feasible and a good fit for agent products because approvals, input boundaries, and similar host-dependent decisions are naturally pause points.

Reporting should be separate:

- reporters receive non-blocking activity such as streamed text deltas, tool start/end, usage, warnings, and compaction notices
- reporters must not control loop progress
- hosts that want raw observability can attach a reporter that stores every event

The loop should not:

- decide the system prompt format
- decide when to compact by itself
- decide approval UX
- persist conversation state

Those should be injectable policies.

### 3. Tools

Tooling should be built around a small JSON-schema-first interface because that matches how current providers expose tool calling.

Recommended boundary:

- `ToolSpec`: name, description, JSON schema, optional metadata
- `ToolHandler`: async invocation against typed execution context
- `ToolRegistry`: lookup, registration, discovery
- `ToolResult`: structured content, machine-readable payload, error classification

Typed helper layers can exist later, but the wire format should stay schema-oriented.

This is also why the loop cannot depend only on provider-native message types. It needs a normalized representation of:

- tool calls requested by the model
- tool results returned to the model
- user-visible content blocks
- usage and finish reasons

### 4. Context loaders

`AGENTS.md` and skills do not naturally fit the same bucket as shell or filesystem tools. They are context sources.

V1 should model them separately:

- `ContextSource`: load and normalize prompt/context material
- built-ins for `AGENTS.md`
- built-ins for skills directories
- custom path resolvers

This keeps the tool system focused on runtime actions and keeps prompt/context assembly explicit.

### 5. Permissions and approvals

Permissions should be a first-class cross-cutting concern, not embedded ad hoc into each tool.

V1 should support:

- allow/deny decisions before tool execution
- path scoping for filesystem operations
- command policy for shell execution
- environment variable filtering
- optional approval callbacks for "ask user" flows

The shell tool should support fine-grained controls, but the control model should be shared where possible.

### 6. Compaction

Compaction belongs in v1, but with a narrow scope.

V1 should provide:

- a `Compactor` trait
- compaction triggers and loop integration points
- a normalized `CompactionResult` artifact
- the ability to replace raw transcript segments with summaries or checkpoints

V1 should not provide:

- long-term memory product features
- retrieval pipelines
- opinionated summarization prompts

### 7. Reporting

Reporting should be event-first.

V1 should emit structured events for:

- model turn start/end
- streamed output deltas
- tool invocation start/end
- usage and cost metadata
- errors and denials
- compaction activity

Applications can then plug in:

- stdout reporters
- JSON loggers
- tracing integrations
- usage dashboards

These are observability and presentation hooks, not loop control hooks.

In other words:

- reporting is for non-blocking information flow
- the driver API is for blocking control flow

## Canonical content model

The canonical content model should be provider-neutral but strongly informed by the OpenAI content-block style because that structure is becoming the most reusable common shape.

V1 should define normalized content parts such as:

- input text
- output text delta
- reasoning or summary blocks where available
- tool call request
- tool result
- image, file, or binary references
- usage metadata

The important constraint is:

- `agentkit` owns the normalized content model used by the loop, tools, compaction, and reporting
- adapters are responsible for mapping provider-native messages into that model

So the library can be inspired by OpenAI's model without hardcoding OpenAI response objects as the public core types.

## Proposed workspace layout

The cleanest start is a workspace with small crates plus one umbrella crate for ergonomic re-exports.

```text
agentkit/
  crates/
    agentkit-core
    agentkit-capabilities
    agentkit-loop
    agentkit-tools-core
    agentkit-tool-shell
    agentkit-tool-fs
    agentkit-context
    agentkit-mcp
    agentkit-compaction
    agentkit-reporting
    agentkit
```

### Crate responsibilities

`agentkit-core`

- shared types: messages, events, usage, errors, content blocks, tool call shapes
- no heavy runtime dependencies beyond async fundamentals

`agentkit-capabilities`

- lower-level capability abstractions shared by tools and MCP
- invocable operations
- readable resources
- parameterized prompts

`agentkit-loop`

- session loop orchestration
- tool-call roundtrips
- policy hook integration
- blocking interrupt driver
- event emission and observer fanout

`agentkit-tools-core`

- tool specialization over lower-level invocable capabilities
- tool traits
- registry
- execution context
- execution orchestration
- result types
- permission interfaces shared by tools
- approval/auth interruption contracts

`agentkit-tool-shell`

- shell execution tool
- command filters, cwd rules, env filtering, timeout controls

`agentkit-tool-fs`

- read/write/edit/list operations
- root scoping and operation-level permissions

`agentkit-context`

- `AGENTS.md` loading
- skills loading
- custom search locations and precedence rules

`agentkit-mcp`

- MCP client bridge
- server lifecycle management
- auth plumbing
- transport-agnostic MCP connection management
- MCP capability-provider implementation
- MCP tool adaptation into the shared tool system
- MCP resource and prompt access APIs

`agentkit-compaction`

- compactor traits
- loop hooks
- transcript replacement/checkpoint helpers

`agentkit-reporting`

- reporter implementations and adapters over loop events
- in-memory and logging reporters
- normalized usage aggregation
- optional buffering and tracing adapters

`agentkit`

- umbrella crate
- feature-gated re-exports
- "few lines of code" ergonomic builder layer

## Suggested feature flags

At the umbrella crate level:

- `loop`
- `capabilities`
- `tools`
- `tool-shell`
- `tool-fs`
- `context`
- `mcp`
- `compaction`
- `reporting`

Optional convenience flags:

- `default = ["loop", "capabilities", "tools", "reporting"]`
- `full = ["loop", "capabilities", "tools", "tool-shell", "tool-fs", "context", "mcp", "compaction", "reporting"]`

The default feature set should stay small. If `full` becomes the real-world default, the crate split is not doing useful work.

## Recommended public API style

The v1 public API should feel like library assembly, not framework inheritance.

Rough target:

```rust
use agentkit::prelude::*;

let tools = ToolRegistry::new()
    .with(ShellTool::new(shell_policy))
    .with(FileSystemTool::new(fs_policy));

let agent = Agent::builder()
    .model(my_model_adapter)
    .tools(tools)
    .context(AgentsMd::default())
    .context(Skills::from_dir(".agent/skills"))
    .reporter(JsonReporter::stdout())
    .build()?;
```

That should be enough to wire a useful agent product together once the app provides:

- its own model adapter
- its own approval policy
- its own prompts
- its own storage or transcript policy

## V1 tool boundary

Built-ins worth shipping in v1:

- filesystem
- `AGENTS.md` loader
- skills loader
- MCP bridge
- a simple custom-tool extension path

Likely useful but not required for v1:

- shell
- HTTP/fetch tool
- git tool
- patch/diff helper tool

The right standard is: ship the parts that nearly every serious coding agent needs and that benefit from shared safety primitives. Do not ship a giant "tools zoo".

## The shortest path to value

The most important success case for v1 is:

"A developer can build a minimal coding agent by implementing one provider adapter and selecting a few prebuilt components."

If that requires a lot of framework ceremony, the design is too abstract.
If that requires `agentkit` to own provider implementations or UI, the design is too broad.

## Open questions to resolve early

Most of the first-round boundary questions are now resolved in the subsystem docs.

The remaining meaningful question is:

1. Which built-in helper policies should ship in the first public release versus later follow-up crates?

Everything else currently has a working design direction:

- `agentkit-core` stays runtime-agnostic
- provider-native types stay behind adapter boundaries, with normalized projections for the rest of the stack
- v1 ships with a comprehensive first-class multimodal content and delta model
- `agentkit-loop` stays runtime-agnostic and uses a blocking-interrupt driver with synchronous observational events
- reporting is layered on loop events
- permissions are action-based and ternary
- tools and MCP share a public lower-level capability layer
- custom tools are the primary extension path; custom capabilities are the advanced escape hatch
- MCP uses a transport-agnostic design with pluggable transports
- `agentkit-mcp` owns the bridge from MCP resources/prompts into context-oriented usage
- MCP tools integrate with the shared tool system, while MCP resources and prompts remain first-class MCP concepts
