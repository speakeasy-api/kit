# Context loading

A coding agent needs to understand the project it's working in. This chapter covers `agentkit-context`: how agents load project instructions, conventions, and ambient context into the transcript.

## The problem

Without context, a coding agent is generic. It doesn't know your project's conventions, tech stack, or constraints. It will write Python-style Rust, ignore your linting rules, and miss architectural patterns that are obvious to anyone who has read the README.

Context loading bridges this gap by injecting project-specific information into the transcript before the model sees it:

```text
Without context:

  Transcript: [System("You are a coding assistant"), User("Fix the parser")]
  Model: writes code that doesn't match project conventions


With context:

  Transcript: [
      System("You are a coding assistant"),
      Context("This project uses Rust 2024 edition. Error handling uses thiserror..."),
      Context("All public types must have doc comments. Use `cargo clippy` before committing."),
      User("Fix the parser"),
  ]
  Model: writes idiomatic code that follows project conventions
```

Once system prompts and context items are stable, they form a reusable prefix for every turn. The next chapter covers prompt caching — the transport optimization that exploits this stability.

## ContextLoader

The loader combines multiple context sources and produces `Vec<Item>` with `ItemKind::Context`:

```rust
let items = ContextLoader::new()
    .with_source(AgentsMd::discover_all(workspace_root))
    .with_source(my_custom_source)
    .load()
    .await?;
```

Sources are loaded in registration order and their results are concatenated. The resulting items are ordinary transcript entries — the loop and providers don't need a separate context path. They're preloaded via `AgentBuilder::transcript` alongside the system items, and the first user message is preloaded via `AgentBuilder::input`:

```rust
let mut transcript = Vec::new();
transcript.extend(system_items);
transcript.extend(context_items); // ← loaded by ContextLoader

let agent = Agent::builder()
    .model(adapter)
    .transcript(transcript)
    .input(user_items)
    .build()?;

let mut driver = agent.start(session_config).await?;
```

## AgentsMd

The primary built-in source loads `AGENTS.md` files (similar to how Claude Code uses `CLAUDE.md` or Cursor uses `.cursorrules`):

```rust
// Find the nearest AGENTS.md by walking up from the workspace
let source = AgentsMd::discover(workspace_root);

// Find all AGENTS.md files from root to workspace (stacked)
let source = AgentsMd::discover_all(workspace_root);
```

### Discovery modes

```text
AgentsMdMode::Nearest — stop at the first match:

  /home/user/projects/myapp/AGENTS.md     ← found, stop
  /home/user/projects/AGENTS.md           (not checked)
  /home/user/AGENTS.md                    (not checked)


AgentsMdMode::All — collect everything, outermost first:

  /home/user/AGENTS.md                    ← loaded first (general)
  /home/user/projects/AGENTS.md           ← loaded second (more specific)
  /home/user/projects/myapp/AGENTS.md     ← loaded last (most specific)
```

The `All` mode is useful for organizations that layer context: a company-wide `AGENTS.md` at a parent directory, project-level instructions at the repo root, and module-specific instructions in subdirectories. More specific instructions appear later in the transcript and take precedence in the model's attention.

### Configuration

```rust
let source = AgentsMd::discover_all(workspace_root)
    .with_file_name("CLAUDE.md")            // Custom file name
    .with_search_dir(".agent/")             // Check sidecar directories
    .with_path("/team/shared/AGENTS.md");   // Explicit file path
```

| Method            | What it does                                       |
| ----------------- | -------------------------------------------------- |
| `with_file_name`  | Change from `AGENTS.md` to a different name        |
| `with_search_dir` | Check a specific directory (no ancestor walk)      |
| `with_path`       | Include an explicit file path (skipped if missing) |

Explicit paths and search dirs are checked before ancestor discovery. All results are deduplicated by path.

### Loaded item structure

Each loaded file becomes an `Item` with metadata:

```rust
Item {
    kind: ItemKind::Context,
    parts: [Part::Text(TextPart {
        text: "[Loaded AGENTS]\nPath: /workspace/AGENTS.md\n\n<file contents>",
        ...
    })],
    metadata: {
        "agentkit.context.source": "agents_md",
        "agentkit.context.path": "/workspace/AGENTS.md",
    },
}
```

The metadata lets compaction strategies and reporters identify where context came from. The `source` key distinguishes `AgentsMd` items from other context sources.

## The `ContextSource` trait

All context loading goes through a simple trait:

```rust
#[async_trait]
pub trait ContextSource: Send + Sync {
    async fn load(&self) -> Result<Vec<Item>, ContextError>;
}
```

`AgentsMd` implements this trait. Custom sources implement it to load context from any source.

## Context vs System items

`ItemKind::Context` is distinct from `ItemKind::System` because they serve different purposes:

|            | `ItemKind::System`             | `ItemKind::Context`                   |
| ---------- | ------------------------------ | ------------------------------------- |
| Example    | "You are a coding assistant"   | "This project uses Rust 2024 edition" |
| Origin     | Hardcoded by the application   | Loaded from project files             |
| Scope      | Same across all projects       | Different per project                 |
| Mutability | Never changes during a session | May be refreshed on context reload    |
| Compaction | Preserved during compaction    | May be summarized or refreshed        |

This distinction matters for compaction:

- **System items** are always preserved — they define the agent's identity
- **Context items** might be refreshed (reload from disk) or summarized during compaction

## Writing custom context sources

The `ContextSource` trait is simple enough that custom sources are straightforward:

```rust
struct GitBranchContext;

#[async_trait]
impl ContextSource for GitBranchContext {
    async fn load(&self) -> Result<Vec<Item>, ContextError> {
        let output = tokio::process::Command::new("git")
            .args(["branch", "--show-current"])
            .output()
            .await
            .map_err(|e| ContextError::ReadFailed {
                path: PathBuf::from(".git"),
                error: e,
            })?;

        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(vec![Item {
            id: None,
            kind: ItemKind::Context,
            parts: vec![Part::Text(TextPart {
                text: format!("Current git branch: {branch}"),
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
        }])
    }
}
```

Register it alongside other sources:

```rust
let items = ContextLoader::new()
    .with_source(AgentsMd::discover_all(workspace_root))
    .with_source(GitBranchContext)
    .load()
    .await?;
```

Other useful custom sources:

- Load dependency versions from `Cargo.toml` or `package.json`
- Load CI configuration summaries
- Load recent git log entries
- Load MCP resources (via `ResourceProvider`)
- Load team-specific conventions from a shared server

> **Example:** [`openrouter-context-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-context-agent) demonstrates context loading from `AGENTS.md` and skills directories.
>
> **Crate:** [`agentkit-context`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-context) — depends on [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core) and [`async-fs`](https://docs.rs/async-fs) for filesystem operations.
