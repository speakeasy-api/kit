# agentkit docs

This directory holds the design and implementation-facing documents for `agentkit`.

Start here:

- [`v1-boundary.md`](./v1-boundary.md): the proposed product boundary, crate split, feature flags, and core abstractions for v1.
- [`v1-plan.md`](./v1-plan.md): the recommended implementation order and the docs backlog to grow alongside the codebase.
- [`architecture.md`](./architecture.md): the current crate layout and runtime control flow.
- [`getting-started.md`](./getting-started.md): entry points, example progression, and minimal assembly shape.
- [`feature-flags.md`](./feature-flags.md): umbrella crate feature flags and typical combinations.
- [`core.md`](./core.md): the proposed scope and API contract for the `agentkit-core` crate.
- [`compaction.md`](./compaction.md): compaction triggers, strategy pipelines, backend hooks, and loop integration.
- [`capabilities.md`](./capabilities.md): the lower-level capability abstraction shared by tools and MCP.
- [`context.md`](./context.md): built-in `AGENTS.md` and skills loading behavior.
- [`loop.md`](./loop.md): the proposed operational model and public API for the `agentkit-loop` crate.
- [`reporting.md`](./reporting.md): the proposed design for the `agentkit-reporting` crate and event-consumer adapters.
- [`tools.md`](./tools.md): the proposed design for `agentkit-tools-core` and the built-in tool boundaries.
- [`mcp.md`](./mcp.md): the proposed design for the `agentkit-mcp` integration crate.
- [`acp.md`](./acp.md): the proposed design for the `agentkit-acp` runtime integration and ACP approval resolver.
- [`permissions.md`](./permissions.md): the proposed shared policy and approval model across tools and MCP.

The intent is to keep `agentkit` narrow: a reusable Rust toolkit for building agent applications, not a full hosted platform or a single opinionated agent product.
