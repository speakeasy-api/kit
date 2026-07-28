# agentkit v1 plan

## Goal

Produce a small, coherent Rust toolkit that can support a coding-agent-style application without forcing provider, UI, or storage choices.

## Phase order

### Phase 0: lock the boundary

Deliverables:

- confirm the product statement
- confirm crate split
- confirm v1 non-goals
- confirm the initial content/event model
- confirm the driver-style loop API, blocking interrupt model, and pause/resume semantics
- confirm the lower-level capability abstraction shared by tools and MCP

Docs to write:

- `docs/v1-boundary.md`
- `docs/architecture.md`
- `docs/provider-adapter.md`

Exit criteria:

- we can describe what belongs in `agentkit` in one paragraph
- we can explain why stateful and stateless providers fit the same abstraction
- we can explain the normalized content model without referring to any one provider's wire types

### Phase 1: core and loop

Deliverables:

- workspace skeleton
- `agentkit-core`
- `agentkit-capabilities`
- `agentkit-loop`
- event types
- session and model adapter traits
- driver API with `next()` yielding only blocking interrupts and turn completion
- loop integration tests with a fake model

Docs to write:

- `docs/architecture.md`
- `docs/loop.md`
- `docs/events.md`

Exit criteria:

- a fake provider can stream output, request a tool, receive the result, and finish a turn
- a host can answer an approval request and resume the same loop instance
- usage and errors are represented in the event stream
- non-blocking reporting does not require the host to manually drive every event

### Phase 2: tool system and essential built-ins

Deliverables:

- `agentkit-tools-core`
- filesystem tool
- a minimal custom-tool extension path with low boilerplate
- permission model
- approval hook interface

Docs to write:

- `docs/tools.md`
- `docs/shell-tool.md`
- `docs/filesystem-tool.md`
- `docs/permissions.md`

Exit criteria:

- a host can register custom tools with minimal boilerplate
- filesystem tools can be locked down by policy

### Phase 3: context and MCP

Deliverables:

- `agentkit-context`
- `AGENTS.md` loading with custom search paths
- skills loading with custom directories
- `agentkit-mcp` with auth hooks, server lifecycle management, tools, resources, and prompts
- MCP-owned adapters that expose MCP resources/prompts through the capability layer for context use

Docs to write:

- `docs/context.md`
- `docs/agents-md.md`
- `docs/skills.md`
- `docs/mcp.md`

Exit criteria:

- a host can add project instructions and skills without hardcoding path logic
- MCP tools can be surfaced through the same registry as native tools
- MCP resources and prompts can be exposed through stable `agentkit` abstractions

### Phase 4: compaction and reporting

Deliverables:

- `agentkit-compaction`
- `agentkit-reporting`
- usage aggregation
- transcript checkpointing hooks
- JSON/logging reporters

Docs to write:

- `docs/compaction.md`
- `docs/reporting.md`
- `docs/usage.md`

Exit criteria:

- the loop can trigger compaction without owning the compaction policy
- a host can observe usage, tool activity, and failures from structured outputs

### Phase 5: umbrella crate and end-to-end example

Deliverables:

- `agentkit` umbrella crate with feature-gated re-exports
- ergonomic builder APIs
- one end-to-end example app using a fake provider

Docs to write:

- `docs/getting-started.md`
- `docs/examples/minimal-agent.md`
- `docs/feature-flags.md`

Exit criteria:

- a new user can understand the library from the example without reading every internal crate first

## Suggested first code milestone

If the goal is to move from design to code quickly, the first useful milestone is narrower than the full v1:

1. `agentkit-core`
2. `agentkit-capabilities`
3. `agentkit-loop`
4. `agentkit-tools-core`
5. one fake tool
6. one fake provider adapter

That is enough to validate the central claim: one loop abstraction can support tool-calling agents without knowing a real provider implementation.

## Recommended docs structure

Once code starts landing, `docs/` should stay small and navigable:

- `docs/README.md`
- `docs/v1-boundary.md`
- `docs/architecture.md`
- `docs/getting-started.md`
- `docs/loop.md`
- `docs/tools.md`
- `docs/permissions.md`
- `docs/context.md`
- `docs/mcp.md`
- `docs/compaction.md`
- `docs/reporting.md`
- `docs/examples/`

Avoid adding one-off design notes unless they capture a real decision. If a document does not explain an API or a boundary, it will decay quickly in an empty repo.

## Risks to manage

### Risk 1: over-generalizing the provider abstraction

If the model adapter API becomes too abstract, simple stateless providers become awkward.

Mitigation:

- design around a session abstraction
- prove it with one fake stateless adapter and one fake stateful adapter before locking APIs
- keep provider-native types behind the adapter boundary and validate the normalized projection layer separately

### Risk 2: mixing product policy into the toolkit

If prompts, approvals, or storage assumptions leak into core crates, the toolkit becomes hard to reuse.

Mitigation:

- treat these concerns as injected interfaces
- require each crate README or module doc to state what policy it does not own

### Risk 3: tool safety becomes inconsistent

If each built-in tool invents its own permission model, hosts cannot reason about risk coherently.

Mitigation:

- define shared permission concepts early
- make shell and filesystem tools adopt them first

### Risk 4: compaction gets too ambitious

If compaction tries to become memory, retrieval, and summarization all at once, it will slow the whole project down.

Mitigation:

- keep compaction to loop hooks and transcript replacement artifacts in v1

## Recommendation

The next concrete step should be to turn the boundary doc into code-facing docs, then create the workspace skeleton around that plan.

The highest-value immediate docs are:

1. `docs/architecture.md`
2. `docs/provider-adapter.md`
3. `docs/tools.md`
4. `docs/permissions.md`

Those four documents will force the important API shapes into the open before implementation starts.
