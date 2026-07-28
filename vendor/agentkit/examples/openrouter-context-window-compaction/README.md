# `openrouter-context-window-compaction`

Demonstrates token-aware compaction driven by the per-item `Usage` that the
loop now stamps onto every assistant `Item`.

`agentkit-compaction` ships `context_window_trigger(window, percent)`, a
ready-made `TriggerFn` that walks the transcript looking for the most recent
reported `input_tokens` and fires when it crosses
`window * percent / 100`. Older items are summarised into a single `Context`
item by `AgentCompactor`, which runs a nested loop over the same OpenRouter
adapter.

## How it works

```
provider response  ──►  Item { usage: Some(Usage), .. } appended to transcript
                            │
                            ▼
                  context_window_trigger walks transcript on AfterTurnEnded
                            │
                            ▼
        fires when last reported input_tokens ≥ window * percent / 100
                            │
                            ▼
                  SummarizeOlderStrategy + AgentCompactor backend
                            │
                            ▼
                  nested loop summarises older items → Context item
```

The compactor is registered as a `LoopMutator` via
`AgentBuilderCompactorExt::compactor`; the loop runs it at
`MutationPoint::AfterTurnEnded`, so by the time the second turn starts the
trigger has already seen usage from turn 1.

## Why a small-context model?

The example is pinned to **`mistralai/mistral-7b-instruct-v0.1`** (4096-token
window) so the trigger fires on a realistic-sized prompt instead of needing
megabytes of filler. The first user turn carries a ~10 KB short story
(`src/story.txt`, embedded via `include_str!`) and asks the model to
summarize it. The reported `input_tokens` for that turn cross the 60%
threshold; the next turn runs compaction before sending its request,
replacing the original story with the assistant's summary so follow-up
questions still work.

If the slug is unavailable on your OpenRouter account, override
`OPENROUTER_MODEL` and `MAX_CONTEXT_TOKENS` to match a different small-window
model.

## Run

Requires `OPENROUTER_API_KEY` (see `OpenRouterConfig::from_env`).

```bash
cargo run -p openrouter-context-window-compaction
```

Tunable env vars (all optional):

| Variable             | Default | Meaning                                             |
| -------------------- | ------- | --------------------------------------------------- |
| `MAX_CONTEXT_TOKENS` | `800`   | Trigger budget (see "A note on token counts" below) |
| `CONTEXT_PERCENTAGE` | `60`    | Fire at this % of the budget                        |

`OPENROUTER_MODEL` is intentionally **ignored** — the example pins
`mistralai/mistral-7b-instruct-v0.1` so the trigger budget matches a
known small-context endpoint.

## A note on token counts

OpenRouter normalizes `usage.prompt_tokens` for cost comparability across
providers, and the reported value can be much smaller than what native
tokenization of the same prose would suggest. For example, the embedded
~10 KB story tokenizes to roughly 2.5 K tokens with mistral's BPE, but
OpenRouter reports ~580 `prompt_tokens` on the free
`mistralai/mistral-7b-instruct-v0.1` endpoint.

The trigger compares against whatever the provider reports, so
`MAX_CONTEXT_TOKENS` here is a **simulated budget** tuned to the reported
number — not the model's true 4096-token window. If you swap in another
provider/model, retune `MAX_CONTEXT_TOKENS` to match what
`[usage] input_tokens=…` actually shows.

## Notes

- The adapter populates `Usage::tokens.input_tokens` from the provider's
  `prompt_tokens` field, and the loop stamps the resulting `Usage` onto the
  assistant `Item` for every turn. `context_window_trigger` reads it back.
- The strategy pipeline used here drops reasoning parts and failed tool
  results, then summarises older items via `AgentCompactor`'s nested loop
  into a single `Context` item.
- For a real deployment, set `MAX_CONTEXT_TOKENS` to your model's actual
  context window and pick a `CONTEXT_PERCENTAGE` (e.g. `80`) that leaves
  room for the next response.
