# agentkit-compaction

`agentkit-compaction` is the optional transcript replacement layer. It plugs into the loop through a single, generic seam — `LoopMutator` from `agentkit-loop` — so compaction is just one shape of transcript edit alongside redaction, repair, and instrumentation.

## Core types

- `Compactor`
  - object-safe trait combining "should this fire?" and "rewrite the transcript"; implements `LoopMutator` automatically
- `CompactionStrategy`
  - applies one transcript transformation
- `CompactionPipeline`
  - composes multiple strategies in order
- `CompactionBackend`
  - optional host-provided semantic summarization backend
- `CompactionContext`
  - gives strategies access to the optional backend
- `CompactionRequest`
  - transcript, reason, metadata
- `CompactionResult`
  - replacement transcript plus metadata
- `CompactionReason`
  - why compaction was triggered
- `SummaryRequest` / `SummaryResult`
  - typed backend contract for semantic summary strategies

## Included helpers

- `StrategyCompactor`
  - bundles a trigger closure + strategy (+ optional backend) into a `Compactor`
- `context_window_trigger(window, percent)`
  - fires when the latest item's reported `input_tokens` ≥ `window * percent / 100`
- `item_count_trigger(max_items)`
  - fires when the transcript exceeds `max_items`
- `DropReasoningStrategy`
  - removes reasoning parts and optionally drops empty items
- `DropFailedToolResultsStrategy`
  - removes failed tool-result parts and optionally drops empty items
- `KeepRecentStrategy`
  - keeps the last N non-preserved items
- `SummarizeOlderStrategy`
  - summarizes older removable items through the optional backend
- `AgentCompactor`
  - `CompactionBackend` that runs a nested loop over a sub-agent to summarise items

## Loop integration

Compactors register as mutators on the agent builder via the `AgentBuilderCompactorExt::compactor` extension:

```rust
use agentkit_compaction::{
    AgentBuilderCompactorExt, CompactionPipeline, DropFailedToolResultsStrategy,
    DropReasoningStrategy, KeepRecentStrategy, StrategyCompactor,
};

let compactor = StrategyCompactor::builder()
    .item_count_trigger(12)
    .strategy(
        CompactionPipeline::new()
            .with_strategy(DropReasoningStrategy::new())
            .with_strategy(DropFailedToolResultsStrategy::new())
            .with_strategy(
                KeepRecentStrategy::new(8)
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

If a strategy needs semantic summarisation, pass a backend:

```rust
let compactor = StrategyCompactor::builder()
    .trigger(context_window_trigger(200_000, 80))
    .strategy(SummarizeOlderStrategy::new(8))
    .backend(Arc::new(AgentCompactor::builder()
        .agent(inner_agent)
        .session_id("compaction")
        .build()?))
    .build()?;
```

When a compactor fires, `agentkit-loop` emits:

- `AgentEvent::MutationStarted { mutator, point, .. }`
- `AgentEvent::MutationFinished { mutator, dirty, metadata, .. }`

Compactors run at every `MutationPoint` — currently `AfterToolResult` (between tool results and the next inference call) and `AfterTurnEnded`. The trigger closure decides which points are relevant. If a mutator's edit leaves the transcript protocol-invalid (orphaned/duplicate tool_use or tool_result), the loop hard-fails with `LoopError::Mutator` rather than letting the next request blow up at the provider.

## Scope

This crate does not implement:

- memory products
- retrieval
- provider-specific LLM calls
- persistence

It is the hook and artifact boundary for transcript replacement and optional host-provided semantic compaction.
