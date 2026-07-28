# agentkit-context

<p align="center">
  <a href="https://crates.io/crates/agentkit-context"><img src="https://img.shields.io/crates/v/agentkit-context.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-context"><img src="https://img.shields.io/docsrs/agentkit-context?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-context.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Context loaders for workspace-local agent instructions.

This crate provides:

- upward discovery for `AGENTS.md`
- a composable `ContextLoader` for combining multiple sources
- context items that are already shaped for `agentkit-core` transcripts

Use it to inject repository instructions or other local guidance into agent
sessions.

## Example

```rust,no_run
use agentkit_context::{AgentsMd, ContextLoader};
use agentkit_core::{Item, ItemKind};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace_root = std::env::current_dir()?;

    // Load AGENTS.md files from all ancestors and one extra search directory.
    let context_items = ContextLoader::new()
        .with_source(
            AgentsMd::discover_all(&workspace_root)
                .with_search_dir(workspace_root.join(".agent")),
        )
        .load()
        .await?;

    // Context items are ordinary transcript entries (ItemKind::Context).
    // Prepend them to the input alongside a system prompt and user message,
    // then submit everything to the agent loop.
    let mut input = vec![Item::text(ItemKind::System, "You are a helpful assistant.")];
    input.extend(context_items);
    input.push(Item::text(
        ItemKind::User,
        "What are the project guidelines?",
    ));

    println!("transcript has {} items", input.len());
    Ok(())
}
```
