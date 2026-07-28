# agentkit-tool-compose

Lua tool composition for agentkit.

This crate exposes a single `compose` tool. The model supplies a Lua script and
optional JSON input; the script can call the current tool catalog with
`tool(name, input)` and inspect available tools with `tools()`.

```rust
let registry = agentkit_tool_compose::registry();
```

Compose is opt-in. Add this registry explicitly with
`AgentBuilder::add_tool_source`.

For a richer tool description, wrap an existing tool source:

```rust
let tools = agentkit_tool_compose::ComposeTool::wrap(child_source);
```

The wrapped source still advertises and executes its child tools directly, while
`compose` renders child output schemas into its own description. Dynamic sources
remain live: catalog events and child lookups delegate to the wrapped source.

The final compose result enters the transcript as compact JSON by default.
With the `toon` feature enabled,
`ComposeConfig::with_result_encoding(ResultEncoding::Toon)` switches it to
[TOON](https://docs.rs/serde_toon2) (Token-Oriented Object Notation), which
renders uniform object lists as a header plus one row per element — smaller
than JSON for the list-shaped values compose scripts tend to return. The tool
description gains a note explaining the format so the model can read it.
