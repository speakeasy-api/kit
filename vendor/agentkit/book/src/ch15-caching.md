# Prompt caching

Prompt caching reduces cost and latency by reusing stable prefixes of a turn request. This chapter covers the cache model in `agentkit-loop`: what the host configures, what the loop passes to providers, and how adapters translate that into provider-specific behavior.

## Why caching lives at the request level

Caching is a transport optimization, not transcript semantics. The transcript is the conversation itself: system prompts, user messages, tool calls, tool results, and context items. Caching is applied when a turn is sent to a provider.

That distinction is why agentkit models caching on `SessionConfig` and `TurnRequest`, not on `Item` or `Part`.

```rust
pub struct SessionConfig {
    pub session_id: SessionId,
    pub metadata: MetadataMap,
    pub cache: Option<PromptCacheRequest>,
}

pub struct TurnRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub transcript: Vec<Item>,
    pub available_tools: Vec<ToolSpec>,
    pub metadata: MetadataMap,
    pub cache: Option<PromptCacheRequest>,
}
```

The host sets a session-level default. The loop copies that into each `TurnRequest` unless the host overrides the next turn explicitly.

## The cache request shape

The request is provider-neutral:

```rust
pub enum PromptCacheMode {
    Disabled,
    BestEffort,
    Required,
}

pub enum PromptCacheRetention {
    Default,
    Short,
    Extended,
}

pub enum PromptCacheStrategy {
    Automatic,
    Explicit {
        breakpoints: Vec<PromptCacheBreakpoint>,
    },
}

pub enum PromptCacheBreakpoint {
    ToolsEnd,
    TranscriptItemEnd { index: usize },
    TranscriptPartEnd { item_index: usize, part_index: usize },
}

pub struct PromptCacheRequest {
    pub mode: PromptCacheMode,
    pub strategy: PromptCacheStrategy,
    pub retention: Option<PromptCacheRetention>,
    pub key: Option<String>,
}
```

### Field semantics

| Field       | Variant      | Meaning                                                                |
| ----------- | ------------ | ---------------------------------------------------------------------- |
| `mode`      | `Disabled`   | Do not send cache hints for this turn                                  |
|             | `BestEffort` | Use caching if the provider supports it; degrade silently otherwise    |
|             | `Required`   | Fail the turn if the cache request cannot be honored                   |
| `strategy`  | `Automatic`  | Let the adapter use native provider behavior, or emulate it internally |
|             | `Explicit`   | The host specifies concrete cache boundaries                           |
| `retention` |              | Provider-neutral hint for short-lived vs extended retention            |
| `key`       |              | Optional stable cache key for providers that support one               |

## Session defaults

The simplest place to configure caching is the session:

```rust
let mut driver = agent
    .start(SessionConfig {
        session_id: SessionId::new("coding-agent"),
        metadata: MetadataMap::new(),
        cache: Some(PromptCacheRequest {
            mode: PromptCacheMode::BestEffort,
            strategy: PromptCacheStrategy::Automatic,
            retention: Some(PromptCacheRetention::Short),
            key: None,
        }),
    })
    .await?;
```

This says:

- try to use prompt caching
- let the provider or adapter choose the prefix automatically
- prefer short-lived retention
- do not require a user-supplied cache key

### `None` vs `Disabled`

These have different semantics:

| Value                                     | Meaning                                                                                                   |
| ----------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `cache: None`                             | No cache preference — adapters don't add cache fields; provider-native automatic caching may still happen |
| `cache: Some(... { mode: Disabled, .. })` | Explicitly disable cache controls from agentkit for this session or turn                                  |

## Automatic strategy

`PromptCacheStrategy::Automatic` is the recommended default for most applications:

```rust
PromptCacheRequest {
    mode: PromptCacheMode::BestEffort,
    strategy: PromptCacheStrategy::Automatic,
    retention: Some(PromptCacheRetention::Short),
    key: None,
}
```

Why this is the default shape:

- it keeps the host provider-agnostic
- OpenAI-style providers can use native automatic caching
- Anthropic-style providers can be supported by adapters that synthesize explicit cache headers internally
- unsupported providers degrade cleanly in `BestEffort` mode

In other words: the host chooses the policy, not the provider-specific mechanism.

## Explicit strategy

When the host knows the desired boundaries, it can specify them directly:

```rust
let cache = PromptCacheRequest {
    mode: PromptCacheMode::BestEffort,
    strategy: PromptCacheStrategy::Explicit {
        breakpoints: vec![
            PromptCacheBreakpoint::ToolsEnd,
            PromptCacheBreakpoint::TranscriptItemEnd { index: 3 },
        ],
    },
    retention: Some(PromptCacheRetention::Short),
    key: Some("workspace:agentkit".into()),
};
```

Breakpoints are expressed in request order:

1. tools
2. transcript items
3. transcript parts within an item

This matters for providers that expose explicit cache boundaries on tools or message blocks.

## Per-turn overrides

Session defaults are often enough, but the loop also supports per-turn overrides:

```rust
driver.set_next_turn_cache(
    PromptCacheRequest::explicit_required([PromptCacheBreakpoint::tools_end()])
        .with_retention(PromptCacheRetention::Extended)
        .with_key("release-planning"),
)?;

// then submit the user message via the next cooperative interrupt:
input_request.submit(&mut driver, vec![user_item])?;
```

The override applies to the next model turn only. Later turns fall back to the session default. The `set_next_turn_cache` call is independent of input submission, so it composes with whichever `InputRequest` / `ToolRoundInfo` handle is in scope.

## How adapters use it

The loop does not interpret cache semantics itself. It passes the normalized request through to the adapter.

For completions-style providers, the mapping hook is:

```rust
fn apply_prompt_cache(
    &self,
    body: &mut serde_json::Map<String, Value>,
    request: &TurnRequest,
) -> Result<(), LoopError>;
```

That gives adapters three implementation choices:

1. use native automatic caching controls
2. synthesize explicit cache headers or request fields from the normalized request
3. ignore unsupported cache requests in `BestEffort` mode, or error in `Required` mode

This is the architectural boundary: agentkit keeps the host-facing API stable while each provider adapter chooses the correct wire format.

## Reporting cache usage

Providers can report cache reads and writes through normalized usage fields:

```rust
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
}
```

- `cached_input_tokens`
  - input tokens served from cache
- `cache_write_input_tokens`
  - input tokens written into cache on this request

This makes caching visible to reporters and host-side cost accounting without exposing provider-specific response formats.

## Practical recommendation

For most hosts, start here:

```rust
SessionConfig {
    session_id: SessionId::new("demo"),
    metadata: MetadataMap::new(),
    cache: Some(PromptCacheRequest {
        mode: PromptCacheMode::BestEffort,
        strategy: PromptCacheStrategy::Automatic,
        retention: Some(PromptCacheRetention::Short),
        key: None,
    }),
}
```

Then reach for explicit breakpoints only when you need to control exact cache boundaries.

> **Crate:** Prompt caching types live in [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core). Session and turn-level cache handling is in [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop). Provider-specific cache mapping is in each [`agentkit-provider-*`](https://github.com/danielkov/agentkit/tree/main/crates) crate.
