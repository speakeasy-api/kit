# agentkit-tool-skills

<p align="center">
  <a href="https://crates.io/crates/agentkit-tool-skills"><img src="https://img.shields.io/crates/v/agentkit-tool-skills.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-tool-skills"><img src="https://img.shields.io/docsrs/agentkit-tool-skills?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-tool-skills.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Progressive Agent Skills discovery and activation for agentkit.

This crate discovers `SKILL.md` files, builds a lightweight catalog for the
model, and exposes an `activate_skill` tool that loads full skill instructions
on demand.

## What it provides

- recursive skill discovery from one or more roots
- frontmatter parsing for skill metadata
- per-session activation tracking to avoid duplicate loads
- progressive disclosure so the model sees descriptions first and bodies later

## Example

```rust,no_run
use agentkit_tool_skills::SkillRegistry;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
// Discover skills from project- and user-level locations
// (`./.agents/skills` and `~/.agents/skills`).
let registry = SkillRegistry::discover(".").build().await;

// `tool_registry()` returns a `ToolRegistry` exposing only `activate_skill`,
// ready to merge with the rest of your agent's tools.
let tools = agentkit_tools_core::ToolRegistry::new()
    .merge(registry.tool_registry());
# Ok(())
# }
```

For explicit roots and filtering, use `SkillRegistry::from_paths` together with
`with_filter` and `discover_skills`:

```rust,no_run
use agentkit_tool_skills::{Skill, SkillRegistry};
use std::path::PathBuf;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let home = std::env::var("HOME")?;
let registry = SkillRegistry::from_paths(vec![
    PathBuf::from("./.agents/skills"),
    PathBuf::from(home).join(".agents/skills"),
])
.with_filter(|skill: &Skill| skill.name != "deprecated-skill")
.discover_skills()
.await;
# Ok(())
# }
```
