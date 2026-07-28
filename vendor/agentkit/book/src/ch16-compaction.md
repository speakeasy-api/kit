# Transcript compaction

Long conversations exceed context windows. Compaction is how you keep an agent session viable without losing important context. This chapter covers `agentkit-compaction`: the trigger, strategy, and pipeline system.

## The design

Compaction is optional and host-configured. It plugs into the loop through the generic `LoopMutator` seam — a `Compactor` is just a mutator that decides "should I rewrite the transcript right now?" and, if yes, swaps it out. There are three concerns:

1. **When** to compact — the trigger closure
2. **How** to compact — the strategy (or pipeline of strategies)
3. **What** to use for semantic summarization — the optional backend

```rust
let compactor = StrategyCompactor::builder()
    .item_count_trigger(12)
    .strategy(strategy)
    .build()?;

let agent = Agent::builder()
    .model(adapter)
    .compactor(compactor) // AgentBuilderCompactorExt
    .build()?;
```

## Triggers

A trigger is a closure with the shape `Fn(&[Item], MutationPoint) -> Option<CompactionReason> + Send + Sync` (aliased as `TriggerFn`). Returning `Some(reason)` fires compaction; `None` skips. Built-in helpers:

- `item_count_trigger(max_items)` — fires when the transcript grows beyond `max_items`
- `context_window_trigger(window, percent)` — fires when the latest item's reported `input_tokens` reaches `window * percent / 100` (only at `AfterTurnEnded`)

Custom triggers are plain closures:

```rust
StrategyCompactor::builder()
    .trigger(Box::new(|transcript, point| {
        (point == MutationPoint::AfterTurnEnded && transcript.len() > 20)
            .then_some(CompactionReason::TranscriptTooLong)
    }))
    .strategy(strategy)
    .build()?;
```

## Strategies

A `CompactionStrategy` transforms the transcript:

```rust
pub trait CompactionStrategy {
    async fn compact(
        &self,
        request: CompactionRequest,
        ctx: &mut CompactionContext,
    ) -> Result<CompactionResult, CompactionError>;
}
```

Built-in strategies:

| Strategy                        | Description                                  |
| ------------------------------- | -------------------------------------------- |
| `DropReasoningStrategy`         | Removes reasoning parts from assistant items |
| `DropFailedToolResultsStrategy` | Removes tool results where `is_error: true`  |
| `KeepRecentStrategy`            | Keeps the last N non-preserved items         |
| `SummarizeOlderStrategy`        | Summarizes older items through the backend   |

### Preservation

`KeepRecentStrategy` supports preservation rules:

```rust
KeepRecentStrategy::new(8)
    .preserve_kind(ItemKind::System)
    .preserve_kind(ItemKind::Context)
```

System and context items are kept regardless of age. Only user/assistant/tool items are subject to trimming.

## Pipelines

Multiple strategies compose into a pipeline:

```rust
CompactionPipeline::new()
    .with_strategy(DropReasoningStrategy::new())
    .with_strategy(DropFailedToolResultsStrategy::new())
    .with_strategy(KeepRecentStrategy::new(8)
        .preserve_kind(ItemKind::System)
        .preserve_kind(ItemKind::Context))
```

Strategies execute in order. Each one receives the output of the previous.

## Semantic compaction

For summarization, the host injects a `CompactionBackend`:

```rust
let compactor = StrategyCompactor::builder()
    .trigger(trigger)
    .strategy(strategy) // e.g. SummarizeOlderStrategy
    .backend(my_backend)
    .build()?;
```

The backend receives a `SummaryRequest` and returns a `SummaryResult`. agentkit ships `AgentCompactor`, a backend that runs a nested loop over a sub-agent (use it directly or roll your own). The [`openrouter-compaction-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-compaction-agent) example wires `AgentCompactor` into the pipeline.

## A compaction example

Before and after a compaction pipeline run:

```text
Before (20 items, trigger threshold: 12):

  [0]  System: "You are a coding assistant"           ← preserved
  [1]  Context: "Project uses Rust 2024..."            ← preserved
  [2]  User: "What files are in src/?"
  [3]  Asst: (reasoning) "Let me list the directory"
             (text) "I'll check..."
             (tool_call) fs_list_directory
  [4]  Tool: ["main.rs", "lib.rs", "parser.rs"]
  [5]  Asst: "There are three files..."
  [6]  User: "Read parser.rs"
  [7]  Asst: (tool_call) fs_read_file
  [8]  Tool: "fn parse() { ... }"
  [9]  Asst: "The parser contains..."
  [10] User: "Add error handling"
  [11] Asst: (tool_call) fs_replace_in_file
  [12] Tool: { is_error: true, "search text not found" }  ← failed
  [13] Asst: "Let me try again..."
             (tool_call) fs_replace_in_file
  [14] Tool: "Replacement successful"
  [15] Asst: (tool_call) shell_exec("cargo check")
  [16] Tool: "Compiling... 0 errors"
  [17] Asst: "Done! I added error handling..."
  [18] User: "Now add tests"
  [19] Asst: (thinking about tests...)


Pipeline:
  1. DropReasoningStrategy     → removes reasoning parts from [3], [19]
  2. DropFailedToolResultsStrategy → removes failed result [12]
  3. KeepRecentStrategy(8, preserve System+Context)

After (10 items):

  [0]  System: "You are a coding assistant"            ← preserved
  [1]  Context: "Project uses Rust 2024..."             ← preserved
  [2]  Asst: "Let me try again..."                      ← recent 8 start here
             (tool_call) fs_replace_in_file
  [3]  Tool: "Replacement successful"
  [4]  Asst: (tool_call) shell_exec("cargo check")
  [5]  Tool: "Compiling... 0 errors"
  [6]  Asst: "Done! I added error handling..."
  [7]  User: "Now add tests"
  [8]  Asst: (now without reasoning part)
```

The model lost the early conversation but retains the system prompt, project context, and the most recent work. This is usually a good trade-off — the model's attention is strongest on recent items anyway.

## Compaction vs prompt caching

Compaction and prompt caching both operate on the turn request, but they optimize for different things:

- **Prompt caching** tries to reuse an unchanged serialized prefix from earlier turns
- **Compaction** deliberately changes the serialized transcript to make it shorter

That means compaction often invalidates the cache prefix even when the conversation is still logically continuous.

Consider the actual prompt prefix sent to the provider:

```text
Before compaction:

  [system]
  [context]
  [user 1]
  [assistant 1]
  [tool result 1]
  [user 2]
  [assistant 2]
  [user 3]

  cacheable prefix for turn N:
  └───────────────────────────────────────────────┘


After compaction:

  [system]                       ← still present
  [context]                      ← still present
  [compaction summary]           ← new item, replaces older history
  [assistant 2]
  [user 3]

  new cacheable prefix for turn N+1:
  └─────────────────────────────┘
```

Provider-side caches are keyed on the exact prompt prefix, not the semantic meaning of the conversation. These changes all tend to invalidate an existing cache entry:

- dropping reasoning parts
- removing failed tool results
- trimming old user/assistant/tool items
- replacing many old items with a single summary item
- reordering or refreshing context items

### What survives compaction

After compaction, only the compacted transcript is part of future conversation history from the model's perspective.

| Retained                                       | Dropped                                       |
| ---------------------------------------------- | --------------------------------------------- |
| Preserved `System` items                       | Reasoning blocks                              |
| Preserved `Context` items                      | Failed tool results                           |
| Recent user/assistant/tool items that survived | Older conversation items past the keep window |
| Summary items from semantic compaction         | Raw items replaced by a summary               |

The provider-side cache itself is not conversation history — it is transport state owned by the provider. It can accelerate reuse of a prompt prefix, but it does not extend the model's memory. If compaction removes or rewrites earlier items, those items are gone from the request even if an older provider cache entry still exists.

### The trade-off

Compaction can reduce cache hit rates in exchange for keeping the session under the context window.

That trade-off is often still correct:

- without compaction, the session may stop fitting at all
- with compaction, the transcript becomes shorter and cheaper even if an old cache prefix is no longer reusable
- preserved system/context prefixes still give the cache some stable surface area

In practice:

- structural compaction usually causes smaller cache disruptions
- semantic compaction causes larger cache disruptions because it replaces many items with a new summary
- long-lived context items and stable tool schemas are still good cache anchors

This does not mean all caching efficiency is lost after compaction. The typical sequence:

1. the old cacheable prefix becomes invalid because the transcript changed
2. the compacted transcript is sent on the next turn
3. that new, shorter transcript becomes the new cacheable prefix
4. subsequent turns reuse the compacted prefix until the next compaction cycle

Compaction behaves like a cache reset followed by a new stable baseline.

```text
turn N-1:
  long history prefix                          ← cached

turn N:
  compaction runs
  compacted transcript sent                    ← old cache no longer matches

turn N+1, N+2, N+3:
  same compacted transcript prefix reused      ← new cache hits accumulate
```

This is one reason semantic compaction can still be efficient overall. The summary item may replace a large unstable history with a much smaller durable prefix that is cheap to resend and easy to cache for the next several turns.

This is why caching is configured separately from compaction in agentkit. Compaction decides what the transcript should be. Caching then operates on whatever transcript remains. For the cache model itself, see [Chapter 15](./ch15-caching.md).

## Loop integration

Compactors register as `LoopMutator`s. The loop runs every registered mutator at each `MutationPoint` — `AfterToolResult` (between tool results and the next inference call) and `AfterTurnEnded` (after the assistant final, interrupt, or cancellation). The trigger decides which points are relevant.

When a compactor fires:

1. The compactor emits `AgentEvent::MutationStarted { mutator, point, .. }` with a stable label it chose
2. The strategy pipeline transforms the transcript through the cursor
3. The loop validates transcript invariants (tool_use ↔ tool_result pairing) and hard-fails with `LoopError::Mutator` on a protocol violation
4. The compactor emits `AgentEvent::MutationFinished { mutator, dirty, metadata, .. }`

```text
Turn lifecycle with a registered compactor:

  next() → merge pending input
       │
       ▼
  begin model turn
       │
       ▼
  tool_result? ─► run mutators at AfterToolResult ─► continue turn
       │
       ▼
  turn ends ─► run mutators at AfterTurnEnded
       │
       ▼
  next turn sees the post-mutation transcript
```

The model never observes raw compaction artifacts — it just sees the post-mutation transcript.

### Compaction is not summarization

Most compaction strategies are structural — they drop parts or trim items without understanding semantics. `DropReasoningStrategy` removes reasoning blocks because they're verbose and not needed for future turns. `KeepRecentStrategy` drops old items because the model's attention is weakest on them.

Only `SummarizeOlderStrategy` (with a `CompactionBackend`) does semantic work — it summarizes old items into a shorter form. This requires an LLM call, which adds latency and cost. The [`openrouter-compaction-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-compaction-agent) example uses a nested agent loop as the summarization backend.

> **Example:** [`openrouter-compaction-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-compaction-agent) demonstrates all three types: structural (drop reasoning), hybrid (keep recent + summarize older), and semantic (nested-agent summarization backend).
>
> **Crate:** [`agentkit-compaction`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-compaction) — depends on [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core). The loop integration is in [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop).
