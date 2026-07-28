# One Round-Trip Instead of N: A Case Study on Tool Composition for LLM Agents

**Benchmarking the `compose` Lua tool against granular tool-calling across six
life-like agent tasks and ten models — the one-paragraph change that took its
adoption from 0% to 95% of cells, and the task-shape × model-capability
conditions under which composition actually pays.**

_agentkit, June 2026. Reproduction: `benchmarks/compose-bench` in this
repository. Follow-up: [The Familiarity Tax](runlet-case-study.md) benchmarks
a purpose-built composition language (Runlet) against Lua on the same harness
and decomposes where the cost of an out-of-distribution language actually
goes._

---

## Abstract

Agents with shell access habitually reach for Bash to compose multi-step work:
one pipeline replaces many tool round-trips. agentkit's `compose` tool claims
to bring that economics to tool surfaces a shell cannot reach — MCP servers
and built-in tools — by letting the model write a sandboxed Lua script that
calls `tool(name, input)` N times inside a single model round-trip.

We built a benchmark of six deterministic, life-like scenarios (helpdesk
triage, revenue aggregation, incident investigation, CRM cleanup, calendar
scheduling, and a file-based config migration) and measured wall time, model
round-trips, token consumption, peak context, provider-reported cost, and
rubric-scored accuracy across three arms: granular tools only, the same tools
plus `compose`, and (where reachable) a Bash-only reference.

Three findings. First, **capability without adoption is worthless**: with a
mechanics-only tool description, the model never chose `compose` unprompted,
even though both surfaces were available. Rewriting the description to state
_when_ to use the tool and _why_ it is cheaper — plus an eight-line example —
flipped adoption to 6/6 scenarios with no change to prompts or the tool
itself, and the fix generalized: across a ten-model sweep, 56 of 59 valid
model×scenario cells used compose unprompted. Second, on the initial model
(`claude-sonnet-4.5`) the thesis held across the board: `compose` reduced cost
by **38–77%**, model round-trips by **25–43%**, and wall time by **6–57%**
versus granular calling, while _raising_ accuracy in the three scenarios where
the granular arm made transcription errors under load; on the file-based
scenario it matched the Bash arm's cost within noise — shell-pipeline
economics, delivered to a surface the shell cannot touch. Third, the
multi-model sweep showed the unconditional version of the thesis does **not**
generalize: composition's value is conditional on task shape and model
capability. For N+1 fan-out work it is universal (10/10 models cheaper, −72%
mean cost, accuracy up); for frontier models it is broadly true; for mid-tier
models it inverts into an _accuracy rescue at a cost premium_; and for
exploratory investigation tasks it is an anti-pattern (+107% mean cost).

---

## 1. Background

### 1.1 The round-trip tax

Every tool call in a conventional agent loop costs one model round-trip: the
model emits a call, the runtime executes it, the result is appended to the
transcript, and the _entire growing context_ is resubmitted for the next
decision. For a task that needs N tool interactions this costs:

- N (or N/k, with k-way parallel calling) inference passes of increasing size,
- O(N) intermediate results permanently resident in context,
- N opportunities for the model to mis-transcribe a value it saw 20 results
  ago.

Practitioners have long observed that agents with shell access sidestep this
tax instinctively: rather than calling a file-read tool 12 times, they write
one `sed` loop. The shell is a _composition surface_ — and models prefer it
when it is available.

### 1.2 The compose tool

MCP servers and built-in registry tools have no shell. agentkit's `compose`
tool (`crates/agentkit-tool-compose`) closes the gap: the model submits a Lua
5.4 script which runs sandboxed (no `io`/`os`/`require`/`load`, instruction
limits, nested-call budget) and may invoke any visible tool synchronously via
`tool(name, input)`. The script's return value — and only that — enters the
transcript. `ComposeTool::wrap(source)` additionally renders every child tool's
`output_schema` into the compose description, so the model knows the exact
shape `tool(...)` will return without a discovery call. Because `wrap` accepts
any `ToolSource`, dynamic catalogs such as MCP remain live: child lookups and
catalog-change events delegate to the wrapped source.

### 1.3 Thesis

> Agents prefer Bash-with-composition over granular tools because composition
> is more efficient; `compose` brings that benefit to MCP servers and built-in
> tools.

Decomposed into testable claims:

- **H1 (preference):** offered both surfaces with a neutral prompt, models
  will route multi-step work through `compose`.
- **H2 (efficiency):** the compose arm uses fewer model round-trips, fewer
  tokens, less wall time, and less money than the granular arm.
- **H3 (parity or better):** accuracy does not degrade, and compose approaches
  the Bash arm's numbers where both are possible.

---

## 2. Methodology

### 2.1 Harness

`benchmarks/compose-bench` drives a live model (via OpenRouter; all runs below
use `anthropic/claude-sonnet-4.5`) through agentkit's real agent loop — the
same `Agent`/`LoopDriver` machinery applications use, not a simulation. A
`LoopObserver` collects metrics from the event stream: the completions adapter
emits exactly one `UsageUpdated` event per model request, giving an exact
round-trip count and per-request token figures.

Per run we record:

| metric                     | definition                                                               |
| -------------------------- | ------------------------------------------------------------------------ |
| wall time                  | end-to-end seconds for the run                                           |
| model requests             | API round-trips (one `UsageUpdated` each)                                |
| tool calls / compose share | top-level calls; nested calls inside a Lua script intentionally excluded |
| total tokens               | Σ input + cached input + output across all requests                      |
| peak context               | largest single request — how full the window got                         |
| cost                       | OpenRouter-reported USD (`usage.cost`)                                   |
| accuracy                   | 0–1 rubric, scored mechanically (§2.3)                                   |

Safety rails: a model-request cap (default 60) and a wall-clock timeout
(default 600 s) bound runaway runs; permission prompts are auto-approved so no
arm is penalized by interactive gating. Full transcripts — including every Lua
script the model writes — are persisted per run.

### 2.2 Arms

| arm        | tool surface                                                                                                          |
| ---------- | --------------------------------------------------------------------------------------------------------------------- |
| `granular` | scenario tools only                                                                                                   |
| `compose`  | the same tool source wrapped by `ComposeTool::wrap` — compose **and** every granular tool remain individually visible |
| `bash`     | `shell_exec` only; file-backed scenario only                                                                          |

Two design decisions matter for validity:

1. **The system prompt is identical in every arm** and neutral: it asks for
   efficiency and a final `submit_result` call, and never mentions compose.
   Whatever routes the model toward compose must therefore live in the tool
   catalog itself. The compose arm measures _preference_, not compliance.
2. **The compose arm does not remove the granular tools.** The model is free
   to ignore compose entirely — which, as §3 shows, is exactly what it did at
   first.

### 2.3 Scenarios and scoring

Six scenarios, each a deterministic in-memory world behind mock tools (fresh
per run), shaped after real SaaS workflows:

| scenario            | shape                                                | difficulty mechanism                                                                                                                 |
| ------------------- | ---------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| support-triage      | read + targeted writes                               | 3-field predicate (open ∧ >7 days ∧ body mentions refund); bodies only visible via `get_ticket`; distractors on every predicate axis |
| revenue-report      | read-only N+1 aggregation                            | 40 orders: list gives only ids; status/amount need `get_order`, region needs `get_customer`; refunded/pending/off-month noise        |
| log-incident        | read-only investigation                              | sustained error burst vs per-service background noise; correlate burst onset with deploy timestamps; owner lookup                    |
| crm-hygiene         | write-heavy normalization                            | 18 phone E.164 rewrites + 4 company backfills across 24 contacts; already-valid records must remain untouched                        |
| calendar-scheduling | read-only constraint solve                           | 4 people × 5 days of availability; earliest common 60-minute slot on a 30-minute boundary                                            |
| config-migration    | file-backed migration (only `bash`-capable scenario) | rename `timeout_ms` → `request_timeout_ms` incl. nested keys; `connect_timeout_ms` present as a substring trap                       |

Accuracy is never judged from prose. Every scenario ends with a
`submit_result` tool call carrying a structured answer; the scorer combines
world-state assertions (did exactly the right tickets get escalated? do the
files on disk match the expected transform byte-for-byte?) with answer checks,
using partial credit. Ground truth is recomputed from the same fixture code,
and six unit tests pin each fixture's interesting properties (e.g. the
calendar test re-derives the earliest common slot from the busy intervals, so
a fixture edit cannot silently invalidate the expected answer).

### 2.4 What "life-like" buys

The scenarios deliberately reproduce the shapes that make real agent work
expensive: pagination, list-then-detail (N+1) access patterns, predicates
spanning multiple endpoints, many small mechanical writes, and traps that
punish careless text processing. They are small enough to run for cents but
structured enough that a 40-item N+1 sweep is genuinely painful to do one
round-trip at a time.

---

## 3. Experiment 1: the adoption failure

First run — log-incident, compose arm, original tool description:

> "Run a sandboxed Lua script that composes available tools through
> tool(name, input). The script sees a global `input` … and may call `tools()`
> to enumerate the visible tool catalog at runtime. Return any Lua value to
> make it the compose result." _(+ a list of child output schemas)_

**Result: 9 tool calls, 0 through compose.** Accuracy 1.00, $0.052 — the task
succeeded, but the composition surface was dead weight in the catalog.

The transcript explains part of why. The model fired four `search_logs` calls
_in a single assistant turn_ — parallel tool calling — so 9 calls cost only 5
round-trips. The model already owns a composition mechanism it was trained on,
and it used it. Compose's marginal value concentrates in _sequentially
dependent_ chains (call k+1 needs call k's output), which a short investigation
barely has.

The deeper reason is distributional. Models are post-trained on enormous
volumes of direct JSON tool calls, and on Bash as _the_ canonical composition
surface. The thesis's premise — "agents prefer Bash because composition is
efficient" — has the causality at least partly backwards: they prefer Bash
because it is massively in-distribution. A novel Lua meta-tool has no such
prior, and a model cannot _feel_ token costs or round-trip latency, so
efficiency alone never pulls it toward an unfamiliar tool. Selection is
pattern-matching: "find which service spiked" matches `search_logs`; nothing
in the task matches "run a sandboxed Lua script."

Hypothesis for the fix: **the description sells mechanics and never the
benefit.** It says what compose _is_, not when to reach for it or what it
saves.

---

## 4. The intervention

One change, confined to `compose_description()` in
`crates/agentkit-tool-compose/src/lib.rs`. The description now leads with a
routing rule and a quantified benefit, and ends with a worked example:

> "Run a sandboxed Lua script that composes available tools through
> tool(name, input). **Prefer this tool whenever a task takes more than two
> tool calls: iterating over list results, paginating, fetching details per
> item, filtering or aggregating tool output, or chaining reads into writes.
> The whole script executes in a single round-trip — one compose call replaces
> N individual calls — and only the script's return value enters the
> conversation, so intermediate results never consume context.** …
>
> Example — scan every page, drill into matches, return only the summary:
>
> ```lua
> local page, hits = 1, {}
> repeat
>   local r = tool('list_items', { page = page })
>   for _, it in ipairs(r.items) do
>     if it.status == 'open' then hits[#hits + 1] = tool('get_item', { id = it.id }) end
>   end
>   page = page + 1
> until page > r.total_pages
> return { count = #hits, items = hits }
> ```

No prompt changed. No tool behavior changed. No scenario changed.

Re-running the gate scenario (log-incident, compose arm): the model now routed
the heavy work — the paginated ERROR sweep across all four services — through
one compose script structurally identical to the example (repeat/until over
pages, accumulate, return a table), then made cheap direct calls for the two
singleton lookups. Adoption achieved; gate passed; full matrix unlocked.

---

## 5. Experiment 2: full matrix

All six scenarios, all arms, one repetition, `anthropic/claude-sonnet-4.5`,
no simulated tool latency.

### 5.1 Headline: compose vs granular

| scenario            | Δ wall   | Δ model reqs | Δ total tokens | Δ cost   | Δ accuracy |
| ------------------- | -------- | ------------ | -------------- | -------- | ---------- |
| revenue-report      | **−57%** | −33%         | **−53%**       | **−77%** | **+0.33**  |
| calendar-scheduling | −30%     | −33%         | −14%           | −50%     | 0.00       |
| config-migration    | −20%     | −25%         | −8%            | −50%     | +0.15      |
| crm-hygiene         | −19%     | −33%         | −11%           | −38%     | 0.00       |
| log-incident        | −10%     | −43%         | −17%           | −58%     | 0.00       |
| support-triage      | −6%      | −33%         | −7%            | −41%     | +0.14      |

Compose was chosen in **6/6 scenarios** and won on every metric in every
scenario. Raw cells:

| scenario            | arm      | wall s | reqs | tool calls (compose) | total tokens | peak ctx | cost $ | accuracy |
| ------------------- | -------- | ------ | ---- | -------------------- | ------------ | -------- | ------ | -------- |
| support-triage      | granular | 22.4   | 6    | 17 (0)               | 17,868       | 4,382    | 0.0715 | 0.86     |
| support-triage      | compose  | 21.1   | 4    | 3 (2)                | 16,567       | 4,854    | 0.0419 | **1.00** |
| revenue-report      | granular | 42.3   | 6    | **63** (0)           | 31,445       | 9,064    | 0.1435 | 0.67     |
| revenue-report      | compose  | 18.0   | 4    | 3 (2)                | 14,840       | 4,341    | 0.0336 | **1.00** |
| log-incident        | granular | 23.2   | 7    | 9 (0)                | 22,830       | 4,424    | 0.0808 | 1.00     |
| log-incident        | compose  | 20.9   | 4    | 4 (1)                | 19,053       | 5,304    | 0.0336 | 1.00     |
| crm-hygiene         | granular | 30.2   | 6    | 24 (0)               | 20,007       | 5,493    | 0.0887 | 1.00     |
| crm-hygiene         | compose  | 24.4   | 4    | 3 (2)                | 17,759       | 5,152    | 0.0553 | 1.00     |
| calendar-scheduling | granular | 28.1   | 6    | 14 (0)               | 15,829       | 4,067    | 0.0747 | 1.00     |
| calendar-scheduling | compose  | 19.6   | 4    | 3 (1)                | 13,681       | 4,199    | 0.0374 | 1.00     |
| config-migration    | granular | 42.2   | 8    | 28 (0)               | 40,248       | 7,407    | 0.1626 | 0.85     |
| config-migration    | compose  | 33.8   | 6    | 5 (3)                | 37,048       | 7,498    | 0.0806 | **1.00** |
| config-migration    | **bash** | 27.9   | 6    | 5 (—)                | 19,891       | 5,186    | 0.0778 | 1.00     |

### 5.2 The N+1 showcase: revenue-report

The granular arm did what granular arms must: 4 list pages, then `get_order`
×40, then `get_customer` per completed order — **63 tool calls**. Parallel
calling compressed this into 6 round-trips, but every result still landed in
context (9k peak), and the model then had to do bookkeeping arithmetic over
~60 in-context values. It got one region wrong: accuracy 0.67 at $0.14.

The compose arm's first script naively looped `get_customer` per order and
**hit the 64-nested-call budget**:

> `tool execution failed: compose exceeded 64 nested tool calls`

The model read the error and rewrote the script _with a customer cache_
(memoizing `get_customer` by id), bringing the call count under budget, and
returned exact totals: accuracy 1.00 at $0.034 — **4.3× cheaper, 2.3× faster,
and correct**. The resource limit didn't just bound damage; it pushed the
model toward the algorithmically better program. (Both scripts are reproduced
in Appendix A.)

### 5.3 Accuracy: the under-reported benefit

The pitch for composition is usually efficiency. The data says the stronger
argument may be _correctness_. The granular arm lost points in exactly the
three scenarios with the highest call volume:

- **revenue-report (0.67):** arithmetic over ~60 in-context tool results;
  one region total wrong.
- **support-triage (0.86):** one qualifying ticket missed during a 17-call
  read/update sweep.
- **config-migration (0.85):** one of twelve files diverged from the expected
  byte-exact transform.

A Lua loop does not get tired at item 23 of 40. Moving the iteration out of
the transcript and into a program eliminated the entire class of
"mis-transcribed a value seen 20 results ago" errors: compose scored 1.00 in
all six scenarios.

### 5.4 The Bash anchor

config-migration is the one scenario where the thesis's reference point is
directly measurable, and the ordering is exactly as predicted:

```
granular fs tools : $0.163   42.2 s   accuracy 0.85
compose (fs tools): $0.081   33.8 s   accuracy 1.00
bash (shell_exec) : $0.078   27.9 s   accuracy 1.00
```

Compose landed within ~4% of Bash's cost — pipeline economics on a registry
surface — while the granular arm paid double and still made an error. Bash
retains an edge in tokens (its `sed`-style operations don't round-trip file
contents through the script), which is the honest residual gap: compose
matches Bash's _round-trip_ economics, not always its _payload_ economics.

### 5.5 Judicious routing

A reasonable fear: an aggressive description ("prefer this whenever a task
takes more than two calls") would cause compose to swallow everything,
including work where a direct call is clearer. That did not happen. Across
all six compose-arm runs the pattern was consistent: loops, pagination, and
fan-out went through compose; cheap singletons (`list_services`,
`get_deploys`, `get_service_owners`, `submit_result`) stayed direct. The
model treated compose as a _batch lane_, not a replacement interface.

---

## 6. Experiment 3: multi-model generalization

### 6.1 Setup

Ten models spanning four frontier families and the open-weight ecosystem, all
through the same harness, prompts, and scenarios — one repetition per cell,
131 runs, **$5.05 total**:

`claude-sonnet-4.6`, `claude-haiku-4.5`, `gpt-5.2`, `gpt-5.4`, `gpt-5.5`,
`gpt-5-mini`, `gemini-3.1-pro-preview`, `kimi-k2.6`, `glm-5`,
`deepseek-v4-pro`.

Two configuration changes from Experiment 2. The compose nested-call budget
was raised from the crate default (64) to 256 via `--compose-max-nested`, so
that arms compare _composition_ rather than error-recovery skill — a naive
full-fan-out script for revenue-report needs 76 calls, and leaving the budget
below that would have punished weaker models for a fixture-sized constant.
And cells cut short by infrastructure rather than by the model — two
`kimi-k2.6` runs truncated by the 600 s wall-clock cap on a slow provider
night, one `gpt-5-mini` run killed by a dropped provider response (re-run
cleanly as a replacement cell) — are excluded from the comparison means and
flagged in the full grid.

### 6.2 Adoption generalizes completely

**56 of 59 valid compose-arm cells (95%) used compose unprompted**, across
every family. The improved description is not tuned to one model's routing
habits. The three abstentions are themselves informative: two of the three
are log-incident (`claude-sonnet-4.6`, `glm-5` chose direct calls for the
investigation task — correctly, as §6.4 shows), and one is `deepseek-v4-pro`
on support-triage.

### 6.3 Efficiency does not generalize unconditionally

Per-model means across valid scenario pairs (compose arm vs granular arm):

| model                  | adoption | Δ cost   | accuracy granular → compose |
| ---------------------- | -------- | -------- | --------------------------- |
| claude-sonnet-4.6      | 5/6      | **−58%** | 0.94 → 1.00                 |
| kimi-k2.6              | 4/4      | **−50%** | 1.00 → 0.92                 |
| gpt-5.5                | 6/6      | **−19%** | 1.00 → 1.00                 |
| glm-5                  | 5/6      | +13%     | 0.94 → 1.00                 |
| gpt-5.4                | 6/6      | +17%     | 0.57 → 0.80                 |
| deepseek-v4-pro        | 5/6      | +19%     | 1.00 → 0.97                 |
| gpt-5-mini             | 6/6      | +53%     | 1.00 → 0.85                 |
| gemini-3.1-pro-preview | 6/6      | +55%     | 0.86 → 1.00                 |
| gpt-5.2                | 6/6      | +75%     | 1.00 → 0.90                 |
| claude-haiku-4.5       | 6/6      | +118%    | 0.83 → 1.00                 |

The headline split: frontier models with strong code-generation (sonnet-4.6,
gpt-5.5, kimi-k2.6) realize the cost thesis; mid-tier and small models _pay
more_ under compose — but look at the accuracy columns before reading that as
a loss. For haiku-4.5, gemini-3.1-pro, and gpt-5.4, the granular arm is the
one failing tasks (0.83, 0.86, 0.57), and compose lifts all three to 0.80–1.00.
For those models composition is not a cost optimization; it is an **accuracy
rescue purchased at a cost premium** — haiku's granular arm scored 0.00 on
calendar-scheduling while its compose arm iterated eleven scripts to a correct
answer at 5.8× the price.

### 6.4 Task shape decides, model capability modulates

The same data cut by scenario, across all models:

| scenario            | shape                     | mean Δ cost | mean Δ accuracy | compose cheaper |
| ------------------- | ------------------------- | ----------- | --------------- | --------------- |
| revenue-report      | N+1 fan-out               | **−72%**    | +0.17           | **10/10**       |
| support-triage      | sweep + targeted writes   | −13%        | +0.01           | 7/10            |
| crm-hygiene         | bulk mechanical writes    | +23%¹       | +0.10           | 7/10            |
| config-migration    | one-shot file transform   | +12%        | −0.21²          | 5/9             |
| calendar-scheduling | constraint solve          | +97%        | **+0.22**       | 4/9             |
| log-incident        | exploratory investigation | **+107%**   | −0.10           | 5/10            |

¹ mean skewed by two iteration-loop outliers; the median model saves money.
² driven almost entirely by one family's Lua bugs (§6.5).

Three regimes:

- **The fan-out regime (universal win).** Paginate, fetch per item,
  aggregate: every model, regardless of tier, completes it cheaper under
  compose — and more accurately, because granular arms doing 40–63 calls
  drop values in transcript bookkeeping. This is precisely the access
  pattern MCP's list/detail API conventions generate, and it is where the
  original thesis is simply true.
- **The program-correctness regime (capability-gated).** config-migration
  and calendar-scheduling compress into one nontrivial program. Strong
  models one-shot it and win; weak models either iterate scripts (paying
  the premium) or ship a buggy program (paying in accuracy). Composition
  _concentrates_ risk that granular calling spreads across observable steps.
- **The exploration regime (anti-pattern).** log-incident is
  observe-then-act work: what to fetch next depends on what the data shows.
  Models that scripted it anyway paid +107% on average for −0.10 accuracy;
  gpt-5.2 spent 6.2× granular cost iterating scripts it did not need. The
  two strongest abstentions in the sweep (sonnet-4.6 and glm-5 declining
  compose _for this scenario specifically_) were correct routing decisions.

### 6.5 Family-specific failure modes

**OpenAI pre-5.5 writes buggy Lua text-processing.** On config-migration,
gpt-5.2 scored 0.42, gpt-5.4 scored 0.12, and gpt-5-mini 0.42 in the compose
arm — all from incorrect Lua string-pattern replacements (the
`connect_timeout_ms` substring trap claimed several) — while their granular
and bash arms passed. gpt-5.5 resolved it completely: 1.00 in every cell,
compose cheaper in five of six scenarios. The generational arc
(0.42 → 0.12 → 1.00) is not monotonic, but the current generation closes the
gap, which suggests the failure was a training-coverage issue rather than
anything structural.

**Small models turn one-shot composition into a debugging loop.** haiku-4.5
and gpt-5-mini frequently needed 4–16 compose calls where frontier models
needed 1–2, converting the round-trip savings into a round-trip _spend_. The
loop usually converges to a correct answer (accuracy 1.00 for haiku
everywhere granular also succeeded, plus the calendar rescue), so the failure
mode is economic, not behavioral.

**Wall-time comparisons across providers are unreliable.** kimi-k2.6 ran at
~14 output tok/s through its OpenRouter provider during the sweep — slow
enough that two runs hit the 600 s cap. Requests, tokens, cost, and accuracy
are durable cross-model metrics; wall time is contaminated by provider
throughput and should only be compared within a model.

---

## 7. Analysis

**Adoption is a documentation problem before it is a capability problem.**
The entire 0% → 100% adoption swing came from ~120 words and an example. No
weights changed, no prompts changed. Tool descriptions are the routing layer
of an agent system, and models route by pattern-matching task descriptions
against tool descriptions. A description that only explains mechanics
("runs a sandboxed Lua script") gives the matcher nothing to bind to; the
winning description names the _task shapes_ that should trigger it
("iterating over list results, paginating, fetching details per item…") and
states the payoff in the currency the loop actually spends (round-trips,
context).

**The example is load-bearing.** The model's first real compose script (the
log-incident error sweep) was structurally a transcription of the
description's example — same `repeat/until` pagination idiom, same
accumulate-and-return shape. For an off-distribution surface, the example is
not decoration; it is the few-shot prompt that makes the surface usable.

**Parallel tool calling is the real competitor, and it loses on dependencies
and context.** Where calls are independent, parallel calling matches compose
on round-trips. It cannot pipeline _dependent_ chains (list → detail →
write), and it cannot keep intermediates out of context — every parallel
result lands in the transcript forever. The granular revenue run held all 63
results in context and paid for it twice: in tokens (2.1×) and in a wrong
answer.

**Resource limits double as optimization pressure.** The nested-call budget
turned a naive O(orders) lookup pattern into a memoized one, authored by the
model itself in response to a clear error message. Bounded interpreters with
legible failure modes get self-repair for free.

**Why accuracy improves:** composition moves work from the model's working
memory (attention over a long transcript) into a program's variables. The
program is executed, not remembered. Any task whose failure mode is "lost
track of an item mid-sweep" benefits.

**…and why it sometimes doesn't: composition trades distributed risk for
concentrated risk.** Granular calling spreads correctness across many small,
observable steps — each result is checked by the model before the next
action. A compose script is one commitment: if the program is right, the run
is cheap and exact; if it is wrong (gpt-5.2/5.4's Lua string patterns), the
whole task fails at once, and weaker models recover only by entering a
script-debugging loop that spends the savings. The multi-model data says
which side of this trade you land on is a function of (task shape ×
code-generation capability), not of composition per se.

**Two different products in one tool.** For frontier models, compose is what
the thesis claimed: a cost/latency optimization (−19% to −58% mean cost at
equal-or-better accuracy). For mid-tier models it is something the thesis
never predicted: an accuracy prosthetic — haiku-4.5, gemini-3.1-pro, and
gpt-5.4 all _paid more_ under compose and got dramatically more correct
(+0.14 to +0.23 mean accuracy), because pushing iteration into an executed
program rescued tasks their granular arms were failing outright. Same
mechanism, opposite economics, both valuable.

**Models can out-route the description.** The two strongest models that
declined compose did so precisely on the investigation scenario where
composing is measurably counterproductive — evidence that an aggressive
routing rule ("prefer this for >2 calls") does not override a capable model's
task judgment, while weaker models follow it into the anti-pattern. Routing
guidance in descriptions should state the negative case too.

---

## 8. Threats to validity

These results are promising, not definitive:

1. **n = 1 per cell.** Single repetition throughout; no variance estimates.
   Directional consistency across six scenarios, five metrics, and ten models
   is suggestive — the fan-out result in particular replicates 10/10 — but
   per-cell deltas should not be quoted as point estimates without
   `--reps 3+`.
2. **Description tuned on a test scenario.** log-incident served as the
   adoption gate during description iteration, then appeared in the matrix.
   Its post-fix numbers should be read as in-sample; the other five scenarios
   were untouched during tuning. (That log-incident ended up compose's
   _worst_ scenario argues against tuning having inflated it.)
3. **Zero simulated tool latency.** Mock tools answer in microseconds. Real
   MCP servers add per-call network latency, which compose amortizes and
   granular calling pays N times — so this configuration _understates_
   compose's wall-time advantage. The harness has `--tool-latency-ms` to
   model it.
4. **Wall time is provider-contaminated across models.** OpenRouter routes
   each model through different providers with day-to-day throughput
   variance (§6.5); wall-time comparisons are only meaningful within a
   model. Two latency-truncated cells were excluded from comparison means.
5. **Configuration differs between Experiments 2 and 3.** The multi-model
   sweep raised the compose nested-call budget from 64 to 256, so
   Experiment 2's sonnet-4.5 cells are not directly comparable to
   Experiment 3 cells on scenarios where the budget binds (it cost
   sonnet-4.5 one failed-and-rewritten script on revenue-report).
6. **Self-authored fixtures and rubrics.** Scenarios were designed by the
   same effort that built compose's benchmark, with partial-credit rubrics
   that, while mechanical, embed judgment calls (e.g. 0.6 state / 0.4 answer
   weighting in support-triage).
7. **Nested calls are invisible to the call counter.** `tool_calls` counts
   top-level calls only; compose-arm "3 calls" rows did far more underlying
   work. Cost, tokens, and time are unaffected; raw call-count comparisons
   across arms are not meaningful and are reported only with the compose
   share.
8. **Prompt-cache interaction.** All arms ran with identical automatic
   caching; cache discounts mean reported cost is not a linear function of
   reported tokens, and cache support itself varies by provider.

---

## 9. Implications

For **tool authors**: write descriptions as routing rules, not datasheets.
State the task shapes that should trigger the tool, quantify the benefit in
round-trips/context, and include one canonical example. This is the cheapest
intervention in the entire agent stack — and, in this study, the difference
between a dead feature and a 2–4× cost reduction. State the negative case
too: the multi-model data shows weaker models follow an aggressive routing
rule into scenarios where composing is counterproductive, while only the
strongest models override it.

For **agent-framework designers**: a sandboxed composition surface over the
tool catalog pays for itself fastest on N+1 access patterns, pagination, and
bulk writes — precisely the patterns MCP's list/detail API conventions
generate — and that win is model-universal. Output schemas on child tools
(`ToolSpec::output_schema`) matter: they let the model write a correct script
without discovery calls. Tight, legible resource limits are a feature; models
repair against them. And size the nested-call budget to the data, not the
default: a budget below one full fan-out converts a one-script task into a
multi-round repair loop.

For **agent operators**: which benefit you are buying depends on your model
tier. On frontier models, enabling compose is a straight cost cut. On
mid-tier models, expect to _pay more_ per task and get materially higher
task completion — worth it where correctness dominates spend, wrong where it
doesn't. Avoid routing exploratory/diagnostic work through composition on
any tier.

For **benchmark methodology**: measuring _preference_ (both surfaces
available, neutral prompt) rather than _compliance_ (prompt mandates the
tool) is what surfaced the adoption failure — the most actionable finding in
the study. A compliance-style benchmark would have skipped straight to the
efficiency table and shipped a tool nobody's agent would actually use.

## 10. Future work

- `--reps 3+` for confidence intervals; significance tests on the deltas.
- `--tool-latency-ms 50–200` to quantify the latency-amortization advantage
  under realistic MCP conditions.
- A prompted-compose arm to separate preference from capability ceiling.
- Description ablations: routing rule alone vs example alone vs both, to
  attribute the adoption swing.
- A _routing-guidance_ ablation: §6.4 implies the description should also say
  when **not** to compose ("for exploratory investigation, call tools
  directly") — testable the same way the adoption fix was.
- Larger worlds (hundreds of items) where the granular arm's context growth
  forces compaction — compose's "intermediates never enter context" property
  should compound there.
- Re-run the OpenAI generational arc at `--reps 3+` to confirm the
  0.42 → 0.12 → 1.00 Lua-correctness trajectory is real and not single-run
  noise.

---

## Appendix A: model-authored scripts (revenue-report, compose arm)

**Attempt 1 — rejected by the runtime** (`compose exceeded 64 nested tool
calls`): paginates orders, then per order calls `get_order` and, for completed
orders, `get_customer` — 76 nested calls (4 pages + 40 orders + 32 completed)
against a budget of 64.

```lua
local totals = {NA = 0, EU = 0, APAC = 0}
local page, all_orders = 1, {}
repeat
  local order_list = tool('list_orders', {month = '2026-03', page = page})
  for _, order_summary in ipairs(order_list.items) do
    table.insert(all_orders, order_summary)
  end
  page = page + 1
until page > order_list.total_pages

for _, order_summary in ipairs(all_orders) do
  local order = tool('get_order', {id = order_summary.id})
  if order.status == 'completed' then
    local customer = tool('get_customer', {id = order_summary.customer_id})
    totals[customer.region] = totals[customer.region] + order.amount_cents
  end
end
return totals
```

**Attempt 2 — succeeded**: same structure with a customer memo table,
reducing `get_customer` calls from 32 to at most 18 (one per distinct
customer), landing at ≤62 nested calls — under the budget. Returned exact
region totals.

```lua
local totals = {NA = 0, EU = 0, APAC = 0}
local page, all_orders = 1, {}
repeat
  local order_list = tool('list_orders', {month = '2026-03', page = page})
  for _, order_summary in ipairs(order_list.items) do
    table.insert(all_orders, order_summary)
  end
  page = page + 1
until page > order_list.total_pages

local customer_cache = {}
for _, order_summary in ipairs(all_orders) do
  local order = tool('get_order', {id = order_summary.id})
  if order.status == 'completed' then
    local customer = customer_cache[order_summary.customer_id]
    if not customer then
      customer = tool('get_customer', {id = order_summary.customer_id})
      customer_cache[order_summary.customer_id] = customer
    end
    totals[customer.region] = totals[customer.region] + order.amount_cents
  end
end
return {totals = totals, order_count = #all_orders}
```

## Appendix B: reproduction

Single model (Experiment 2):

```bash
export OPENROUTER_API_KEY=...
export OPENROUTER_MODEL=anthropic/claude-sonnet-4.5

cargo test -p compose-bench          # fixture ground-truth tests
cargo run -p compose-bench --release -- --reps 1
```

Multi-model sweep (Experiment 3): run the same command once per model with
`OPENROUTER_MODEL` set and `--out target/compose-bench-results/multi/<model>`
(the sweep reported here also passed the default `--compose-max-nested 256`),
then merge:

```bash
python3 benchmarks/compose-bench/scripts/cross_model_report.py \
  target/compose-bench-results/multi > cross-model-report.md
```

Outputs per run directory: `runs.jsonl` (raw per-run records), `transcripts/`
(full transcripts including every model-authored Lua script), `report.md`
(aggregate tables and compose-vs-granular deltas). See
`benchmarks/compose-bench/README.md` for flags, arms, and scenario details.
Total cost: ≈ $0.98 for the Experiment 2 matrix; $5.05 for the 131-run,
ten-model sweep in Experiment 3.

The raw per-run records behind every table in this paper are archived in
`benchmarks/compose-bench/results/2026-06-10-multi-model/` (one
`runs.jsonl` per model, plus the merged `cross-model-report.md`).
