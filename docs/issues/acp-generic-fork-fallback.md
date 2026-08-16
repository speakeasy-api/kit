# Generic ACP subagents cannot fork without native `session/fork`

Kit branches a generic ACP subagent only when the agent advertises the ACP `session/fork` capability. If that capability is absent, `fork` returns an unsupported error; only the exact `acp.kit` harness receives Kit's private transcript-cloning fallback.

This leaves otherwise reusable ACP harnesses without a branching path. A generic fallback would need a safe, interoperable snapshot mechanism rather than assuming that a completed prompt can be replayed without changing behavior.

Relevant implementation: `src/tools/subagent.rs` (`Subagents::fork`) and `src/acp_child.rs` (`ChildSession::supports_native_fork`).
