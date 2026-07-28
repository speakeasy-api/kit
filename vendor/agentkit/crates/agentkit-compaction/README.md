# agentkit-compaction

<p align="center">
  <a href="https://crates.io/crates/agentkit-compaction"><img src="https://img.shields.io/crates/v/agentkit-compaction.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-compaction"><img src="https://img.shields.io/docsrs/agentkit-compaction?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-compaction.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Transcript compaction primitives for reducing context size while preserving useful state.

Compaction plugs into `agentkit-loop` through one generic seam — `LoopMutator`. This crate provides:

- **Compactors** (`Compactor`, `StrategyCompactor`) — the mutator-shaped wrapper around triggers and strategies
- **Trigger helpers** (`item_count_trigger`, `context_window_trigger`) that decide _when_ a compactor should fire
- **Strategies** (`DropReasoningStrategy`, `DropFailedToolResultsStrategy`, `KeepRecentStrategy`, `SummarizeOlderStrategy`) that drop, keep, or summarize transcript items
- **Backends** (`CompactionBackend`, `AgentCompactor`) for nested-loop semantic summarisation
- **Pipelines** (`CompactionPipeline`) for composing multiple steps into a single pass

Use it from `agentkit-loop` (or your own driver) when you need to trim older transcript state without losing essential context.

## Quick start

Combine a trigger with a multi-step pipeline and register the result on the builder via `AgentBuilderCompactorExt::compactor`:

```rust
use agentkit_compaction::{
    AgentBuilderCompactorExt, CompactionPipeline, DropFailedToolResultsStrategy,
    DropReasoningStrategy, KeepRecentStrategy, StrategyCompactor,
};
use agentkit_core::ItemKind;

// Build a pipeline that:
//   1. Strips chain-of-thought reasoning parts
//   2. Removes failed tool results
//   3. Keeps only the 24 most recent items (preserving system/context)
let compactor = StrategyCompactor::builder()
    .item_count_trigger(32) // fire once transcript exceeds 32 items
    .strategy(
        CompactionPipeline::new()
            .with_strategy(DropReasoningStrategy::new())
            .with_strategy(DropFailedToolResultsStrategy::new())
            .with_strategy(
                KeepRecentStrategy::new(24)
                    .preserve_kind(ItemKind::System)
                    .preserve_kind(ItemKind::Context),
            ),
    )
    .build()?;

let agent = Agent::builder()
    .model(adapter)
    .compactor(compactor)
    .build()?;
```

## Using a summarization backend

When you want older items to be condensed rather than dropped, pair `SummarizeOlderStrategy` with a `CompactionBackend`. The crate ships `AgentCompactor`, which runs a nested loop over a sub-agent:

```rust
use std::sync::Arc;
use agentkit_compaction::{
    AgentBuilderCompactorExt, AgentCompactor, CompactionPipeline, DropReasoningStrategy,
    StrategyCompactor, SummarizeOlderStrategy, context_window_trigger,
};
use agentkit_core::ItemKind;

let nested = Arc::new(
    AgentCompactor::builder()
        .agent(Arc::new(inner_agent))
        .session_id("compaction")
        .build()?,
);

let compactor = StrategyCompactor::builder()
    .trigger(context_window_trigger(200_000, 80)) // 80% of a 200k window
    .strategy(
        CompactionPipeline::new()
            .with_strategy(DropReasoningStrategy::new())
            .with_strategy(
                SummarizeOlderStrategy::new(16)
                    .preserve_kind(ItemKind::System),
            ),
    )
    .backend(nested)
    .build()?;

let agent = Agent::builder()
    .model(adapter)
    .compactor(compactor)
    .build()?;
```

## Roll your own trigger

Triggers are plain closures with the `TriggerFn` shape — `Fn(&[Item], MutationPoint) -> Option<CompactionReason> + Send + Sync`. The built-ins are just convenience constructors:

```rust
use agentkit_compaction::{CompactionReason, MutationPoint, StrategyCompactor};

let compactor = StrategyCompactor::builder()
    .trigger(Box::new(|transcript, point| {
        if point != MutationPoint::AfterTurnEnded {
            return None;
        }
        (transcript.len() > 10).then_some(CompactionReason::TranscriptTooLong)
    }))
    .strategy(/* ... */)
    .build()?;
```
