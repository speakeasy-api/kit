# The Familiarity Tax: What It Costs an Agent to Speak a Language It Wasn't Trained On

**A follow-up to [One Round-Trip Instead of N](compose-case-study.md).
Benchmarking a purpose-built composition language (Runlet) against
in-distribution Lua inside the same `compose` tool — how to teach a model a
grammar it has never seen, where the 1.6–1.9× cost gap actually comes from
(hidden deliberation, not context), and what TOON result encoding does and
does not buy back.**

_agentkit, July 2026. Reproduction: `benchmarks/compose-bench` in this
repository; raw runs in `benchmarks/compose-bench/results/2026-07-runlet-toon/`._

---

## Abstract

The first study showed that a Lua `compose` tool gives agents shell-pipeline
economics on tool surfaces a shell cannot reach. Lua, however, was chosen for
its distributional familiarity, not its fit: it has no static checking against
tool schemas, no structured concurrency, and its sandbox is a subtraction
exercise. [Runlet](https://github.com/danielkov/runlet) is a small
orchestration language designed for exactly this seam — immutable bindings,
dataflow-implicit concurrency, schema-checked tool calls, bounded fan-out, and
retry boundaries as language constructs. We added it as a second `compose`
backend and asked the uncomfortable question: does a language designed for
agents beat a language agents were trained on?

Four findings. First, **how you present an unfamiliar grammar matters most
exactly where models are weakest**: an A/B of a rules-style primer against a
single fully-annotated exemplar program found the two statistically tied on
`claude-sonnet-4.5`, but on `claude-haiku-4.5` the exemplar halved cost
($0.63 → $0.30 per suite) and raised accuracy (0.72 → 0.83) by collapsing
syntax-error storms roughly four-fold — a worked example beats a specification
for weak models, and costs nothing for strong ones. Below a capability floor
(`gemini-2.5-flash`) no primer form makes the language usable. Second, on a
five-model frontier sweep (150 runs), **syntax stops being the story**: every
frontier model wrote essentially fluent Runlet on the first or second try, two
of five matched Lua's perfect accuracy (and Runlet _beat_ Lua on the one
scenario a frontier model failed), but a structural **1.6–1.9× cost gap
versus Lua persisted for every model**. Third — the central result —
decomposing that gap on `claude-sonnet-5` shows it is **not** primer overhead,
catalog size, or verbosity: visible output is nearly identical (Runlet
programs are 3× _shorter_ than the Lua scripts), but the model spends ~2.6×
more **hidden reasoning tokens** before writing Runlet (79% of billed output
hidden, vs 63% for Lua). The unfamiliarity tax is real, dominant, and
invisible in transcripts — and no in-context intervention can remove it,
because it is paid before the first visible token. Fourth, the addressable
remainder is real: encoding compose results as TOON instead of JSON cut suite
cost by ~9% (−14% on the Lua arm) at identical accuracy, 30/30 runs at 1.00.

The practical bottom line inverts the design intuition: **for today's models,
the in-distribution language is the cost-optimal one, and the purpose-built
language's value shows up as accuracy robustness, not efficiency.** A
language's adoption economics are set by its presence in training data — a
moat that cannot be crossed with a tool description, only narrowed.

---

## 1. Background

### 1.1 Why a second language

The Lua backend won the first study on economics, but Lua inside `compose` is
a compromise:

- **No pre-execution checking.** A Lua script that misspells a tool name or
  projects a nonexistent field fails at runtime, one nested call at a time.
  The first study's §6.5 documented an entire model family shipping buggy Lua
  string-patterns into a scenario trap.
- **No concurrency.** `tool(...)` is synchronous; a 40-item fan-out executes
  serially inside the script. The round-trip win survives, the latency win is
  left on the table.
- **Sandbox by subtraction.** `io`/`os`/`require` removal plus instruction
  caps — a hardening exercise against a general-purpose language.

Runlet approaches the seam from the language side: tool calls are checked
against host-registered input/output schemas _before_ execution; references
create dataflow edges so independent calls run concurrently by default
(`for … limit N` bounds fan-out); bindings are immutable; `boundary retry N
{...} catch err {...}` makes failure handling a value-producing construct; and
the analyzer emits machine-legible diagnostics with fix-its. The language is
small by design: no functions, imports, mutation, or method calls.

The catch is equally structural: **Runlet does not exist in any model's
training data.** The first study found that models route by pattern-matching
and prefer Bash because it is massively in-distribution; the same force that
made compose adoption a documentation problem makes a novel language an
uphill economics problem. This study measures that hill.

### 1.2 Setup

`RunletBackend` implements the same `ComposeBackend` trait as Lua
(`crates/agentkit-tool-compose`, feature `runlet`). The harness gains a
`runlet` arm: identical scenarios, identical neutral system prompt, identical
child tools — only the compose language differs. Five of the six scenarios
run (config-migration is file-backed and exposes `shell`/`fs` tools rather
than a registry catalog); `--reps 3` throughout, so every cell below is a
3-run mean ± sd rather than the first study's single repetitions.

Two loop mechanics matter for interpretation:

- **Diagnostics repair loop.** When a program fails to compile, the model
  receives the analyzer output verbatim — `error RL2102 [unknown callable] at
214..229: …`, `fix:` lines, `did you mean:` suggestions — and retries within
  the same run. Compile rejections therefore cost round-trips, not tasks.
- **Heal pre-pass.** A conservative, insertion-only auto-repair fixes the
  single most common trained-habit error (statement-form `if` around an
  effectful call) before analysis, reporting the rewrite back to the model as
  a `compose_warnings.auto_repaired` note.

---

## 2. Experiment 1: how do you teach a model a grammar?

### 2.1 Two primers

Everything a model ever learns about Runlet arrives in one place: the
`compose` tool description. We tested two forms, at essentially the same
token budget (~5–6 KB):

- **Rules primer** — the conventional approach: a dense specification of the
  grammar ("bindings are immutable", "conditions must be booleans, there is
  NO truthiness", …) with micro-examples per rule and one short worked
  example.
- **Exemplar primer** — a two-sentence framing, then **one ~50-line program
  that exercises the entire grammar** — pagination, concurrent fan-out with
  `skip`, fold aggregation with computed keys, block-if, boundaries,
  intrinsics, fire-and-forget writes — with every constraint stated as a
  comment placed exactly where a model would be tempted to violate it
  (`# bindings are immutable — total = total + x cannot work; fold is the
only way to accumulate`). The program is validated against a mock registry
  in the crate's tests, so it cannot silently rot.

The intuition for the second form has prior art. Grammar-constrained and
grammar-prompted generation ([Wang et al. 2023](https://arxiv.org/abs/2305.19234);
[Synchromesh, Poesia et al. 2022](https://arxiv.org/abs/2201.11227)) show
models can follow an unfamiliar formal syntax when it is made salient;
documentation-retrieval work ([DocPrompting, Zhou et al. 2022](https://arxiv.org/abs/2207.05987))
and the low-resource-language literature ([MTOB, Tanzer et al. 2023](https://arxiv.org/abs/2309.16575);
[survey](https://arxiv.org/abs/2410.03981)) consistently find that _usage
examples transfer better than grammar descriptions_ — a textbook's worked
examples outperform its reference chapters. The first study's own §7 found
the example "load-bearing" for adoption; this experiment asks whether it is
also load-bearing for _correctness_.

### 2.2 The feedback loop is part of the language

Iterating the primer surfaced a design lesson that belongs to the language,
not the prompt. Early calendar-scheduling runs produced a storm of generic
parse errors (37 `RL1008` "expected {" in one three-run cell) traceable to a
single trained habit: models wrote `fold acc = true for u in avail limit 32`,
importing `limit` from the concurrent loop into the sequential fold, and the
parser's generic error pointed at the wrong token. Adding one **targeted
diagnostic** — `RL1020: fold has no limit; fold iterations are sequential by
definition` with a removal fix-it — plus one primer comment at the temptation
site cut the cell's cost from $0.51 to $0.23 in the next run. The general
principle: for a language whose only users are models in a repair loop,
**a targeted diagnostic at a predictable confusion point is worth more than a
paragraph of specification**. Design the errors, not just the grammar.

### 2.3 Tier results: a tie at the top, decisive below

On `claude-sonnet-4.5` (five scenarios × 3 reps, runlet arm only) the two
primers are indistinguishable — every apparent gap dissolved under
replication. Support-triage looked like exemplar 0.80 vs rules 0.40; a
dedicated 5-rep re-run put both primers at exactly **0.52 ± 0.24** — the
scenario's flaky-tool noise, not a primer effect.

On `claude-haiku-4.5` the forms separate sharply:

| primer, haiku-4.5 | suite cost | mean accuracy | parse-error profile (all transcripts) |
| ----------------- | ---------- | ------------- | ------------------------------------- |
| rules             | $0.626     | 0.72          | RL1008 ×71, RL1017 ×58, RL1014 ×56    |
| exemplar          | $0.301     | 0.83          | RL1008 ×19, RL1012 ×6, RL1020 ×4      |

The rules-primer failure mode is stereotyped: statement-form habits
(`RL1014`/`RL1017`: missing `return`, statement `if`) repeated across
retries — haiku reads the rule, violates it, reads the diagnostic, violates
it again. Under the exemplar the same model simply _copies the shape it was
shown_, and the storm collapses ~4×. Calendar-scheduling is the starkest
cell: $0.279 at 0.33 accuracy under rules → $0.140 at 1.00 under the
exemplar.

On `gemini-2.5-flash`, both primers fail (mean accuracy 0.24 rules / 0.30
exemplar; 300–468 `RL1008` per arm; one exemplar revenue-report run burned
742k tokens iterating 40 broken scripts). There is a **capability floor**
below which no presentation of an unfamiliar grammar produces a usable
programmer, mirroring the first study's finding that small models turn
composition into a debugging loop — here the loop simply never converges.

The exemplar primer is now the default (`COMPOSE_RUNLET_PRIMER=rules` restores
the old form): it ties the rules form where models are strong and dominates
where they are weak.

---

## 3. Experiment 2: frontier sweep

Five frontier models, five scenarios, both composition arms, 3 reps — 150
runs, $7.17 total: `claude-sonnet-5`, `claude-opus-4.8`,
`gemini-3.1-pro-preview`, `gpt-5.6-luna`, `glm-5.2`.

| model                  | lua cost | lua acc | runlet cost | runlet acc | runlet/lua |
| ---------------------- | -------- | ------- | ----------- | ---------- | ---------- |
| claude-sonnet-5        | $0.156   | 1.00    | $0.287      | 1.00       | **1.84×**  |
| claude-opus-4.8        | $0.314   | 1.00    | $0.556      | 1.00       | **1.77×**  |
| gemini-3.1-pro-preview | $0.281   | 0.97    | $0.521      | **1.00**   | **1.85×**  |
| gpt-5.6-luna           | $0.048   | 1.00    | $0.076      | 0.91       | **1.59×**  |
| glm-5.2                | $0.059   | 1.00    | $0.093      | 0.88       | **1.57×**  |

Three observations:

**Syntax is solved at the frontier.** Total parse-diagnostic counts across
all 75 runlet runs are in the single-to-low-double digits per model (vs
hundreds at haiku/flash tier), zero heals fired for four of five models, and
most programs compile on the first or second attempt. The primer problem of
§2 is a solved problem for these models.

**Failures moved up the stack — and became family-specific.** The two models
that lose accuracy under Runlet lose it _semantically_, not syntactically.
`gpt-5.6-luna`'s diagnostic profile is dominated by analyzer (not parser)
codes — `RL2311`/`RL2208`, property projections the schema cannot prove —
and it shipped a calendar program whose logic found no valid slot (0.67).
`glm-5.2` dropped crm-hygiene to 0.72 and revenue-report to 0.67 on program
logic. Meanwhile `gemini-3.1-pro` — whose _Lua_ arm was the only frontier
Lua failure (0.87 on support-triage) — scored a perfect 1.00 across all
Runlet cells: the checked language caught what its Lua freestyling missed.
The first study's conclusion that composition "concentrates risk" splits by
language design: Runlet's analyzer moves that risk from runtime to
compile-time for models that respect schemas.

**The cost gap is structural.** Every model, both families that aced it and
families that fumbled it, pays 1.6–1.9× Lua's price for the same tasks at
(mostly) the same accuracy. The consistency across five independent model
families is the tell that this is not a skill issue — which motivated the
decomposition.

### 3.1 Follow-up: Kimi K3

Moonshot's `kimi-k3`, released after the original sweep, ran the same five
scenarios × both arms × 3 reps (30 runs, JSON results):

| model               | lua cost | lua acc | runlet cost | runlet acc | runlet/lua |
| ------------------- | -------- | ------- | ----------- | ---------- | ---------- |
| moonshotai/kimi-k3  | $0.204   | 1.00    | $0.391      | 1.00       | **1.92×**  |

Kimi is the third model to score 1.00 on both arms. Its absolute costs sit in
the middle of the original field, while its 1.92× Runlet premium is just above
the sweep's 1.6–1.9× band. It used composition in 14/15 Lua runs and 13/15
Runlet runs; the two exceptions were `log-incident` runs completed through
granular calls. Syntax was not a problem: there were no parser diagnostics and
three analyzer diagnostics (`RL2103`, unknown property) across the Runlet arm.

Unlike the original models, Kimi reports reasoning tokens natively:

| arm    | billed output | reported reasoning | non-reasoning output | reasoning share |
| ------ | ------------- | ------------------ | -------------------- | --------------- |
| lua    | 25,932        | 8,434              | 17,498               | 33%             |
| runlet | 53,983        | 37,996             | 15,987               | 70%             |

Runlet produced slightly less non-reasoning output but used **4.5× more
reasoning tokens**. This independently confirms the hidden-deliberation
mechanism of §4 on a second provider without estimating visible tokens from
transcript characters.

Four initial runs received immediate upstream 429s at concurrency 6. Each
failed record was replaced by the matching successful serial rerun; provider
failures are excluded from the 30-run result above.

### 3.2 Language-first ablation: host concurrency and assertions

The Kimi transcripts exposed a language-design error: the model read
`for ... limit 32` as a 32-element iteration cap rather than a concurrency
cap. Runlet now removes the source-level number entirely: `for item in items`
always processes the complete collection, while the host configures active
iteration concurrency. It also adds eager `assert(condition, message)`
statements so programs can check invariants without returning checked values.
The existing primer was changed only enough to describe and demonstrate those
two language changes; no pagination or schema-shape teaching was added.

Kimi K3 then ran the two affected scenarios plus `support-triage` as a control,
Runlet arm only, 3 reps each. Values below are per-run means against the §3.1
baseline:

| scenario         | old cost | new cost | Δ cost | old reasoning | new reasoning | Δ reasoning | accuracy |
| ---------------- | -------- | -------- | ------ | ------------- | ------------- | ----------- | -------- |
| crm-hygiene      | $0.130   | $0.078   | −40%   | 4,916         | 1,859         | **−62%**    | 1.00     |
| revenue-report   | $0.070   | $0.056   | −19%   | 2,312         | 1,757         | **−24%**    | 1.00     |
| support-triage   | $0.053   | $0.045   | −16%   | 1,343         | 1,042         | −22%        | 1.00     |
| three-scenario suite | $0.254 | $0.179 | **−29%** | 2,857       | 1,553         | **−46%**    | 1.00     |

The concurrency result is behaviorally clear. Every old revenue run reasoned
about the supposed 32-item ceiling; one split the 40 orders into two loops and
issued a second verification program because exactly 32 happened to be
completed. All three new runs processed all 40 orders directly, with no
chunking or limit deliberation. The exact −24% token estimate remains noisy:
the control also fell 22%, and these are separate three-rep samples rather
than an interleaved A/B.

The assertion result is not yet established. Kimi used `assert` in only one of
three CRM runs. That program checked each update response in-process and
returned only three counts, but aggregate CRM compose-result payload increased
because the other runs returned large discovery values. Assertions are usable;
this run does not show that adding the construct alone reliably changes model
routing or context leakage.

### 3.3 Convergence campaign: v25–v33

The §3.2 result — a 29% suite cut from two language changes — suggested the
1.9× premium was not one monolithic tax but a stack of separable frictions.
We kept pulling the thread on Kimi K3: read the transcripts, name the single
largest remaining friction, fix it at whichever layer owns it (language,
primer, catalog presentation, or harness), rerun only the affected scenarios,
repeat. Fifty-four runs later the suite premium stands at roughly **1.15–1.2×,
with four of five scenarios at or below Lua parity** — down from 1.92×.

The interventions, in order:

**Primer: teach the one-program shape (v25–v27).** Three additions to the
exemplar — a complete pagination idiom (`boundary retry` around each page
fetch, fold-flattened), and two sentences of routing guidance ("keep reads,
transforms, writes, checks, and final submission in one program; do not
return source records for model-side planning; a nested submission call
avoids another round-trip"). Revenue-report fell $0.056 → $0.032 and
support-triage $0.045 → $0.036, both converging to **2 model requests per
run** — one program, one summary, the floor for the loop. CRM followed to
$0.077.

**Measurement before more changes (v28).** Two harness upgrades: every run
record now carries a `diagnostics` field (Runlet error codes seen, with
counts), and full transcripts — including Kimi's native reasoning text —
archive next to `runs.jsonl` instead of dying in `target/`. A fresh 15-run
Lua baseline (the v23 baseline had drifted ~8% upward) and a rerun of the
two cells untouched since v23 produced a failure taxonomy instead of a
hunch: of five runlet compose failures, **four were the scenarios' injected
503s** killing programs whose tool calls were not wrapped in `boundary
retry` — and the transcripts showed models deploy `boundary` exactly where
the exemplar demonstrates it (pagination) and nowhere else. The lone real
language error was `RL2103`: a model wrote `services.items` on a value that
was already the list.

**Catalog presentation is a language surface (v29).** The `.items` error had
a precise cause: output schemas were rendered to the model as raw JSON
Schema, whose `items` *keyword* collides with the `.items` *property
convention* that neighboring paginated tools genuinely follow. The catalog
now renders compact type notation instead (`list_services(): string[]`,
`search_logs(...): { items: {...}[], total_pages: int }`), `RL2103` states
what the value *is* ("not available on this value of type `string[]` —
iterate or index the list itself"), and `list.range` gained the optional
step argument the calendar transcripts showed models re-deriving by hand.
Result: RL2103 extinct across every subsequent run, log-incident $0.066 at
0.89 accuracy → **$0.042 at 1.00**, CRM → $0.067.

**Emit what the world expects back (v30).** v29's calendar cell scored 0.00
at perfect program logic: the step argument pushed models fully numeric, so
they returned `time.format` output — which emitted `14:00:00.000Z`, and the
scorer string-compared against `14:00:00Z`. Both sides were wrong.
`time.format` now renders whole seconds without the fractional part (the
form APIs and validators expect), and the scorer normalizes zero-millisecond
variants. A model-facing language's *output* formats are as much a
compatibility surface as its syntax.

**Schema keywords are droppable; author documentation is load-bearing
(v31).** v30 restored accuracy but every rep burned a repair round-trip on
`RL5213`: `time.parse("09:00")` on busy-interval strings. This class had
never fired before v29 — the regression was ours. The busy intervals'
`HH:MM UTC` format was documented *only* in the schema's `description`
fields, which the v29 notation renderer had dropped as lossy. With
descriptions restored as trailing comments
(`start?: string /* HH:MM UTC */`), calendar converged: **2 requests, zero
failures, zero diagnostics, 1.00 accuracy in all three reps** — no repair
loops left, only deliberation.

**The matched pair (v32/v33).** The type-notation catalog applies to both
arms, so quoting runlet-under-notation against the raw-schema Lua baseline
would flatter runlet. Both arms therefore reran the full suite under the
final configuration, 3 reps each:

| scenario            | lua (v32) | runlet (v33) | ratio | runlet best prior cell |
| ------------------- | --------- | ------------ | ----- | ---------------------- |
| support-triage      | $0.0358   | $0.0386      | 1.08× | $0.0357 (v26)          |
| calendar-scheduling | $0.0328   | $0.0691      | 2.11× | $0.0745 (v30)          |
| log-incident        | $0.0371   | $0.0807      | 2.18× | $0.0421 (v29)          |
| revenue-report      | $0.0244   | $0.0635      | 2.60× | $0.0320 (v26)          |
| crm-hygiene         | $0.0526   | $0.1366      | 2.60× | $0.0668 (v29)          |
| suite               | $0.1827   | $0.3885      | 2.13× | $0.2511                |

Accuracy is 1.00 in all 30 runs. Two findings and one methodological
correction fall out of this table.

First, **the notation catalog is a generic win on the Lua arm too: −17%
suite** against the same-harness v28 baseline ($0.2197 → $0.1827) — larger
than TOON's −14%, and stackable with it. Rendering output schemas as
`{ items: {...}[], total_pages: int }` instead of raw JSON Schema helps
every composer, not just the checked one.

Second, **single-draw suite comparisons cannot resolve the campaign's cost
effect, and this table proves it**. The v33 crm cell ($0.137) is 2.0× the
v29 crm cell ($0.067) — materially the same configuration, different draw:
two of its three reps spent ~7,000 reasoning tokens against v29's ~2,300,
and one revenue rep shipped this campaign's first statement-form parse
storm (RL1008/RL1014/RL1017). The per-cell spread between draws of one
config is as large as most of the effects measured above. The best-prior
column and the v33 column bracket the truth: the suite premium after the
campaign lies somewhere in **1.4×–2.1×**, and pinning it tighter needs a
high-rep matched run, not another 3-rep cell — §3.5 runs one and lands on
**1.77×**. (For calibration, v23's single draw was 1.92× — on cost alone, a
skeptic reading only suite totals could claim the campaign moved nothing.)

What the campaign *did* resolve is structural, and those metrics are
stable across draws: mean model requests fell from ~5.1 (v23) to ~2.9
(v33) with four of five scenarios reaching the 2-request floor in at least
one draw; the RL2103 and RL5213 error classes are extinct; accuracy held
at 1.00 throughout; and every remaining compose failure in v33 is either
an injected 503 on an unwrapped call (calendar ×3, log ×1 — the
boundary-placement teaching remains undone) or a one-shot repaired arity
slip. Round-trips converged; hidden deliberation did not (suite mean
~2,770 reasoning tokens/run in v33 vs ~2,530 in v23) — the model now
writes one bigger program in one sitting, thinking roughly as long in
total, but paying far fewer input-replay and repair costs. The residual
premium is concentrated exactly where §4 predicted: deliberation, now
with the frictions stripped away from around it.

The campaign's larger lesson still stands, restated with the correction:
several defects we had silently booked under "familiarity tax" were really
**presentation defects with names** — a misread concurrency cap (§3.2), a
schema metalanguage collision, dropped documentation, an output format
nobody asked for, un-taught idioms — each found by reading archived
reasoning text, each fixed at the surface that owned it, and one of them
(notation) worth −17% to the *competing* arm. But the matched pair shows
the remaining tax is larger and noisier than the per-cell wins suggested:
presentation work narrows the moat and hardens the structure; it does not
cross the moat.

### 3.4 The spread: three models, three relationships with the tax

To place Kimi's noisy 1.4–2.1× on a spectrum, two further models ran the
full matched suite under the final configuration (both arms × 5 scenarios ×
3 reps each): `deepseek/deepseek-v4-pro` and `x-ai/grok-4.5`, both with
visible reasoning traces.

| model            | lua cost / acc | runlet cost / acc | ratio | reasoning/run |
| ---------------- | -------------- | ----------------- | ----- | ------------- |
| kimi-k3          | $0.183 / 1.00  | $0.389 / 1.00     | 2.13× | ~2,800        |
| deepseek-v4-pro  | $0.030 / 0.73  | $0.051 / 0.92     | 1.69× | ~940 / ~1,520 |
| grok-4.5         | $0.077 / 1.00  | $0.089 / 0.94     | 1.17× | ~370 / ~480   |

Three distinct relationships with the unfamiliar language emerge:

**The premium tracks deliberation habit, not model quality.** Grok barely
deliberates and pays almost no premium (1.17×); Kimi deliberates heavily
and pays 2.13×; deepseek sits between on both axes. The familiarity tax is
not a constant of the language — it is proportional to how much hidden
thinking a model habitually spends per program, which is why no single-model
decomposition (§4, §3.1) could have priced it in general.

**Deepseek reproduces the accuracy-rescue pattern — §3's gemini result at a
budget tier.** Its Lua arm ships wrong answers quietly and cheaply
(support-triage 0.33, calendar and log-incident 0.67 — few requests, no
errors, wrong results), while its Runlet arm storms through parse
diagnostics at haiku-tier rates (RL1001/RL1008/RL1014/RL1017; nine compose
failures in one calendar cell) and lands at 0.92–1.00. The checked language
converts silent wrongness into loud repair loops. At these absolute costs
($0.05/suite) the 1.69× premium buys +0.19 accuracy — by cost-per-correct
task, Runlet is the cheaper arm on this model.

**Grok reveals a failure mode the repair loop cannot see: giving up.** In
one crm run it submitted a program, received parse diagnostics, and simply
stopped — no repair attempt, no submission, accuracy 0.17 in two model
requests. Kimi always repairs; deepseek storms until convergence; grok
abandoned. Repair persistence is a model trait, not a language property —
and a benchmark that only counted diagnostics would file grok as the
*most* fluent of the three (fewest errors) while it quietly forfeits the
task.

One further tier split: grok and Kimi adopt the primer's one-program
routing (2–3.7 requests per run); deepseek does not (5.7–7). Teaching
lands tier-dependently, like the §2.3 primer-form result.

**Steering ablation: does one-pass guidance cause the give-up?** A
plausible mechanism suggested itself: the primer's one-program steering
("…avoids another model round-trip") might bias a cheap model toward
treating the run as one-shot — abandon rather than repair. The steering
paragraph was temporarily env-gated off for the B arm, and grok reran the
give-up scenario at 8 reps per condition. Result: **16/16 runs
repaired and submitted at accuracy 1.00 under both conditions** — the
give-up did not reproduce at all, putting its base rate at roughly 1 in 11
crm runs (the v35 occurrence pooled in) rather than anything steering
selects for. The ablation did cleanly measure what the steering *does* do
on this model: with it, 4.0 requests/run; without it, 5.1 (−21% round
trips) at indistinguishable cost ($0.0275 vs $0.0260 — a cheap model's
extra round-trips are nearly free). Both conditions hit statement-form
parse storms (RL1008/RL1012/RL1014) in most reps and repaired through
them; the v35 abandonment was the outlier, not the norm. The hypothesis
was worth $0.43 to kill: give-up is rare stochastic behavior, and the
steering earns its round-trip savings without measurably causing it.

### 3.5 Normalization: the matched suite at ten reps

The final configuration — including one late teaching change, the exemplar's
fan-out body now demonstrating `boundary retry` around its per-item read —
then ran at 10 reps per cell: 100 runs on Kimi K3, 100 on grok-4.5, and 100
more on grok-4.5 at `reasoning.effort: high` (the adapter gained an
`OPENROUTER_REASONING_EFFORT` passthrough for this; Kimi launched days ago
with a fixed-high setting and no knob yet). All 300 runs are archived with
transcripts.

**Kimi K3, n=10 per cell, every one of the 100 runs at accuracy 1.00:**

| scenario            | lua             | runlet          | ratio     | runlet reasoning/run |
| ------------------- | --------------- | --------------- | --------- | -------------------- |
| support-triage      | $0.0442 ±0.0172 | $0.0353 ±0.0155 | **0.80×** | 1,127                |
| revenue-report      | $0.0280 ±0.0095 | $0.0440 ±0.0264 | 1.57×     | 1,464                |
| crm-hygiene         | $0.0487 ±0.0118 | $0.0951 ±0.0520 | 1.95×     | 3,853                |
| calendar-scheduling | $0.0497 ±0.0153 | $0.0974 ±0.0276 | 1.96×     | 3,547                |
| log-incident        | $0.0364 ±0.0052 | $0.0945 ±0.0847 | 2.60×     | 3,436                |
| suite               | $0.2070         | $0.3663         | **1.77×** | 2,685 (vs lua's 686) |

The normalized post-campaign premium on the heaviest deliberator is
**1.77×** — inside the §3.3 bracket, a real but modest cost improvement on
v23's 1.92× single draw. The structural improvements, by contrast, are
unambiguous at this sample size: the runlet arm ran 13 compose failures
across 50 runs against Lua's 52; requests per run were 2.1–3.9 against
Lua's 4.1–5.3; and the boundary-in-fan-out exemplar change roughly halved
the runlet failure rate versus v33 (0.67 → 0.26 per run) — models copy the
shape where it is demonstrated, one more time. Per-scenario ratios order by
runlet reasoning volume almost perfectly, and the per-cell noise §3.3
complained about resolves into a heavy tail rather than jitter: one
log-incident rep spent 15,081 reasoning tokens ($0.31, zero failures,
correct answer) against a scenario median near 2,300. The residual premium
is deliberation, its variance is deliberation, and its worst case is a
deliberation *spiral* on a correct program.

**Grok-4.5** confirms the other end of the spread at n=10: 1.33× at its
default effort (its lone blemish a *Lua-arm* task forfeit — the §3.4
give-up trait cuts both ways), and 1.31× at `effort: high` with 100/100
accuracy. The effort knob barely moves it — +22% reasoning on the Lua arm,
+46% on runlet, ~150 absolute tokens per run — though the asymmetry is the
deliberation-proportional mechanism in miniature: granted extra thinking
budget, the model spends it disproportionately on the unfamiliar language.
Support-triage stays decisively sub-parity on both models (0.80× Kimi,
0.35–0.44× grok), establishing that for at least one scenario shape — page,
filter, escalate, submit — the purpose-built language is simply cheaper.

The normalized bottom line for the whole study: after every presentation
fix this campaign found, a checked language nobody trained on costs a
light deliberator ~1.3× and a heavy deliberator ~1.8× versus Lua at equal
(perfect) accuracy on frontier-tier models — with the checked arm winning
outright where its one-program shape fits, and the remaining premium
attributable to hidden deliberation with a heavy right tail.

---

## 4. Experiment 3: decomposing the 1.8×

Where does 1.8× go? The candidate explanations: (a) the Runlet primer/catalog
is bigger; (b) Runlet programs or results are more verbose; (c) Runlet runs
take more round-trips; (d) something invisible. Per-request usage records
from the sonnet-5 transcripts settle it.

**Per-run profile, crm-hygiene, sonnet-5 (3-rep means):**

| per run               | lua    | runlet | Δ         |
| --------------------- | ------ | ------ | --------- |
| model requests        | 4      | 5      | +1        |
| input tokens          | 12,416 | 27,972 | +125%     |
| cached input          | 6,783  | 16,490 | +143%     |
| cache-write tokens    | 617    | 1,099  | +482      |
| **output tokens**     | 2,100  | 6,452  | **+207%** |
| script chars written  | 3,566  | 1,093  | **−69%**  |
| result chars returned | 506    | 5,387  | +965%     |

The model writes **3× less visible code** in Runlet and pays **3× more
output tokens** doing it. Cost-weighting the components: output alone
accounts for $0.044 of the $0.057 per-run gap (69% of the runlet cell's
total cost is output tokens at 5× the input price).

**Splitting billed output into visible and hidden, suite-wide (15 runs/arm):**

| arm    | billed output | visible (text + tool calls, est.) | hidden (reasoning) |
| ------ | ------------- | --------------------------------- | ------------------ |
| lua    | 24,533        | ≈9,134                            | ≈15,399 (**63%**)  |
| runlet | 50,867        | ≈10,796                           | ≈40,071 (**79%**)  |

Visible output is nearly identical between arms. The entire output-side gap
is **hidden deliberation**: the model thinks ~2.6× longer before writing a
Runlet program than a Lua script. A single representative request makes it
concrete: the first crm-hygiene runlet turn billed 5,049 output tokens to
produce a 1,638-character tool call (~410 tokens visible) — ≈4,600 tokens of
invisible planning for 25 lines of program.

This is the **familiarity tax**. Lua is muscle memory — pretraining has
burned in its idioms, so a script flows out with minimal deliberation.
Runlet must be _worked out_: the model re-derives the language's rules from
the primer on every single run, in its head, at output-token prices. The tax
has three properties worth stating precisely:

1. **It is invisible in transcripts.** Nothing in the visible output betrays
   it; every prior hypothesis we held (primer size, result verbosity,
   round-trips) targeted the input side, which is second-order.
2. **It is not addressable in-context.** A better primer cannot refund
   deliberation that happens before the first token; only training-data
   exposure amortizes it. (The primer A/B of §2 confirms this at the top
   tier: primer form did not move sonnet-tier cost.)
3. **It is plausibly not pure waste.** The same deliberation that costs 1.8×
   is the likely reason gemini's Runlet arm out-scored its own Lua arm — some
   of the tax buys the robustness. The two cannot be separated with this
   harness (see §7).

The input side is second-order but real and _is_ addressable: Runlet's
larger catalog costs ~+480 cache-write tokens per run, and Runlet programs —
because returning structured data is idiomatic — send back far larger result
payloads (5,387 vs 506 chars/run on crm), which re-enter the context as
input on every subsequent request. That lever is Experiment 4.

---

## 5. Experiment 4: TOON result encoding

### 5.1 Mechanism

The final value a compose script returns enters the transcript as compact
JSON. [TOON](https://docs.rs/serde_toon2) (Token-Oriented Object Notation)
encodes the same value with indentation instead of braces and — the important
case — renders a list of uniform objects as one header plus one
comma-separated row per element:

```text
updates[3]{company,id,name,phone}:
  Initech,c01,Ada Lovelace,+1-555-0100
  Globex,c07,Grace Hopper,+1-555-0107
  Hooli,c12,Alan Turing,+1-555-0112
```

Exactly the shape compose scripts idiomatically return. The encoding is now a
`ComposeConfig` option behind the crate's `toon` feature
(`with_result_encoding(ResultEncoding::Toon)`,
`--result-encoding toon` in the bench), applied identically to both
composition arms; the tool description gains a four-line format note so the
model is never guessing. This is a generic, task-blind intervention — the
scenarios, prompts, and primers are untouched.

### 5.2 Results

Sonnet-5, five scenarios × both arms × 3 reps, versus the Experiment-2 JSON
baseline ($1.21 for the TOON side):

|            | json (v21) | toon (v22) | Δ          |
| ---------- | ---------- | ---------- | ---------- |
| lua arm    | $0.156     | $0.134     | **−14.0%** |
| runlet arm | $0.287     | $0.269     | −6.1%      |
| suite      | $0.443     | $0.404     | **−8.9%**  |
| accuracy   | 1.00       | 1.00       | 0          |

Models parsed tabular TOON without a single comprehension failure — **30/30
runs at accuracy 1.00**, including runs where paginated 8-row tables came
back mid-composition. Where behavior was stable across encodings and payloads
were large (log-incident, ~5.5 KB of results per run), result characters
dropped 18% in both arms and input tokens followed.

Two honesty notes. Per-scenario cost deltas at 3 reps swing ±30–60% because
the hidden-thinking term of §4 dominates cost and is noisy — the suite-level
−9% is the defensible number, not any single cell. And the ceiling is known
in advance: TOON compresses the input-side lever only. It cannot touch the
familiarity tax, which is why the Runlet arm (where thinking dominates
harder) gains less than the Lua arm.

A byproduct of this experiment: the harness gained `--concurrency N`. Runs
are fully independent (each gets a fresh world), so the 30-run suite executes
in ~4 minutes at concurrency 6 instead of 14.5 minutes serially, with no
measurable cross-run interference (all 30 runs scored 1.00).

---

## 6. Analysis

**Training data is the moat, and tool descriptions cannot cross it.** The
first study showed a description can fix _adoption_ — a routing problem. This
study shows it cannot fix _economics_: once the model is writing the
language, its fluency is set by pretraining exposure, and the meter for
non-fluency runs in hidden reasoning tokens at output prices. A language
designed for agents competes not against Lua's design but against Lua's
million-fold presence in the corpus. Any team shipping a DSL for models to
write — query languages, policy languages, workflow definitions — should
budget for this tax, and should measure it the only place it is visible:
billed output minus visible output.

**Worked examples are the accessibility layer of a formal language.** The
exemplar primer did nothing for strong models and halved cost for weak ones.
The mechanism is plain in the diagnostics: weak models cannot reliably
_apply_ stated rules, but they can _imitate_ demonstrated shapes. One
validated program with constraints annotated at temptation sites is worth
more than an equivalent volume of specification — and unlike a
specification, it can be compile-checked in CI so it never drifts from the
language it teaches.

**Design the diagnostics, not just the grammar.** The `RL1020` episode
(§2.2) halved a cell's cost with one targeted error message. In a
model-facing language the compiler _is_ the pair programmer; every
predictable trained-habit collision deserves a diagnostic that names the
habit and hands over the fix. Generic parse errors are a tax on every retry
loop.

**The purpose-built language's real product is robustness, not efficiency —
for now.** Where Lua freestyling failed (gemini support-triage), checked
Runlet succeeded; at haiku tier, Runlet's repair loop converged where
unchecked composition would have shipped wrong answers silently. This is the
first study's "accuracy rescue at a cost premium" appearing one level up the
stack, with the language's analyzer as the rescuer. But two frontier families
also showed the inverse — semantic failures the analyzer cannot catch — so
the robustness claim is conditional on the model respecting schemas, not
universal.

**The efficiency hierarchy for operators is unambiguous.** Lua backend, TOON
encoding, exemplar-style documentation: −14% on the cost-optimal arm at
identical accuracy, all generic. Runlet is the right choice where its
guarantees matter more than 1.6–1.9× on compose-turn cost: catalogs where
schema-checking pre-execution prevents destructive miscalls, latency-bound
fan-outs (Runlet parallelizes inside the script; Lua cannot), or models
known to freestyle.

**Hidden tokens are the missing column in every agent benchmark.** The
decomposition's methodological lesson: input tokens, output tokens, and cost
were all present in our metrics from day one, and all of them mis-attributed
the gap until billed output was split against visible output. Reasoning
models bill deliberation invisibly; any comparison of "how hard is X for a
model" that does not measure hidden output is measuring the wrong thing.

---

## 7. Threats to validity

1. **Three reps, noisy dominant term.** Thinking-token variance produces
   ±30–60% per-cell cost swings; only suite-level and consistent-direction
   findings (the 5/5-family cost gap, the haiku primer split) are robust.
   Single-cell numbers should not be quoted as point estimates.
2. **Visible/hidden split is estimated.** Visible tokens are estimated from
   transcript characters, not tokenizer counts; the 63%/79% figures are
   approximate. The direction and magnitude gap (2.6×) is far larger than
   plausible estimation error.
3. **The original decomposition ran on one model.** Kimi K3's native reasoning
   counts confirm the same mechanism and direction on a second provider, but
   luna and glm bill differently and were not decomposed.
4. **The primer was iterated against these scenarios.** Calendar-scheduling
   in particular served as the diagnostic-design gate (RL1020), so its
   post-fix cells are in-sample. Cross-scenario generalization of the
   exemplar effect rests on the haiku suite, where all five scenarios moved
   together.
5. **TOON was validated at one tier.** Sonnet-5 read TOON flawlessly; the
   models most likely to stumble on an unfamiliar encoding (haiku tier and
   below) were not tested, and §2.3 says unfamiliarity costs most exactly
   there.
6. **Deliberation and robustness are confounded.** The same hidden thinking
   that costs 1.8× may cause the Runlet arm's accuracy wins; this harness
   cannot separate "thinks more because unfamiliar" from "thinks more and
   therefore errs less."
7. **Familiar caveats carry over** from the first study: self-authored
   fixtures and rubrics, provider-contaminated wall times, prompt-cache
   nonlinearity, and flaky-tool noise (support-triage's 0.52 ± 0.24 at
   sonnet-4.5 is scenario noise, not signal).

---

## 8. Implications

For **DSL designers targeting LLM authors**: the language's syntax is the
cheap part. Budget for (a) an exemplar program as the primary documentation
artifact, compile-checked so it cannot rot; (b) targeted diagnostics at every
point where your grammar collides with Python/JS/Lua trained habits — models
repair against legible errors, and each generic error is paid for in retry
round-trips; (c) a familiarity tax of roughly 2–3× hidden deliberation that
no documentation removes. If the language matters enough, the endgame is
training-data presence, not prompt engineering.

For **agent operators**: cost-optimal compose today is Lua + TOON
(−14% at accuracy parity). Choose the checked language when miscall
prevention, in-script concurrency, or freestyle-prone models dominate the
economics — and expect to pay the tax knowingly.

For **benchmark authors**: record billed output _and_ visible output per
request. The single most important number in this study — 79% of Runlet-arm
output is invisible deliberation — was invisible to every aggregate metric
the harness collected by default.

## 9. Future work

- Repeat the native reasoning-token decomposition across more providers and
  models; Kimi K3 confirms the mechanism once, not its universal magnitude.
- TOON at haiku tier and below — the encoding's cheapest win and its highest
  comprehension risk live in the same place.
- A fine-tuning probe: how many Runlet examples does it take to close the
  deliberation gap? Even a small LoRA result would locate the moat's depth.
- Separate deliberation from robustness: force matched thinking budgets
  across arms, or measure Runlet accuracy under a suppressed-reasoning
  configuration.
- Latency-weighted rerun (`--tool-latency-ms 50–200`): Runlet's in-script
  concurrency is unmeasured here because mock tools answer in microseconds —
  the one economic axis where the design should beat Lua outright.
- Exemplar-primer ablation: which annotations carry the haiku-tier effect —
  the program shape, the temptation-site comments, or both?

---

## Appendix A: a model-authored Runlet program

`claude-sonnet-5`, support-triage, first attempt, compiled clean. Pagination
fans out concurrently (`limit 32`), the fold flattens pages, per-ticket
detail fetches run concurrently with `skip`-filtering, and the qualifying
tickets are escalated with fire-and-forget writes inside the loop:

```runlet
first = list_tickets({ status: "open", page: 1 })

pages = for p in list.range(1, first.total_pages + 1) limit 32 {
    r = list_tickets({ status: "open", page: p })
    return r.items
}

all_items = fold acc = [] for pg in pages {
    return acc + pg
}

cutoff = time.parse("2026-06-10T00:00:00Z") - 7 * 86400000

results = for t in all_items limit 32 {
    detail = get_ticket({ id: t.id })
    created = time.parse(detail.created_at)
    is_old = created < cutoff
    has_refund = "refund" in text.lower(detail.body)
    skip if not (is_old and has_refund)
    upd = update_ticket({ id: t.id, priority: "high", add_tags: ["billing-escalation"] })
    return t.id
}

return { results, count: list.length(all_items) }
```

Twenty-five lines, one round-trip, top-level concurrency the equivalent Lua
script cannot express — written after ≈4,600 tokens of invisible thought.

## Appendix B: reproduction

```bash
export OPENROUTER_API_KEY=...
export OPENROUTER_MODEL=anthropic/claude-sonnet-5

# runlet arm (exemplar primer is the default; COMPOSE_RUNLET_PRIMER=rules for the A/B)
cargo run -p compose-bench --release -- \
  --scenarios crm-hygiene,support-triage,revenue-report,log-incident,calendar-scheduling \
  --arms compose,runlet --reps 3 --concurrency 6 \
  --out target/compose-bench-results/runlet-json

# TOON arm
cargo run -p compose-bench --release -- \
  --scenarios crm-hygiene,support-triage,revenue-report,log-incident,calendar-scheduling \
  --arms compose,runlet --reps 3 --concurrency 6 --result-encoding toon \
  --out target/compose-bench-results/runlet-toon
```

The Runlet backend is the `runlet` feature of `crates/agentkit-tool-compose`,
backed by the [`runlet`](https://crates.io/crates/runlet) crate; compose-bench
enables it by default. `COMPOSE_RUNLET_DEBUG=1` prints every model-authored
program to stdout.

Raw per-run records behind every table:
`benchmarks/compose-bench/results/2026-07-runlet-toon/` — one `runs.jsonl`
per (experiment, model, primer/encoding) cell: `v19-*` (primer noise check,
sonnet-4.5), `v20-*` (primer A/B at haiku-4.5 and gemini-2.5-flash), `v21-*`
(frontier sweep, JSON results), `v22-toon-sonnet-5` (TOON A/B),
`v23-moonshot-kimi-k3` (Kimi K3 follow-up), `v24-kimi-k3-language`
(language-first ablation), and the §3.3 convergence campaign:
`v25-kimi-k3-pagination` / `v26-kimi-k3-submit*` / `v27-kimi-k3-one-pass`
(one-program primer teaching), `v28-*` (fresh Lua baseline + stale-cell
rerun under the upgraded harness), `v29-kimi-k3-notation-range`
(type-notation catalog, RL2103, `list.range` step),
`v30-kimi-k3-calendar-timefmt` (`time.format`/scorer fix),
`v31-kimi-k3-calendar-desc` (descriptions restored in notation), and
`v32-kimi-k3-lua-notation` / `v33-kimi-k3-runlet-final` (the matched
final-configuration pair), `v34-deepseek-v4-pro` / `v35-grok-4-5` (the §3.4
cross-model spread), and `v36-grok-steer-on` / `v36-grok-steer-off` (the
one-pass steering ablation), and the §3.5 normalization:
`v37-kimi-k3-10rep`, `v38-grok-4-5-10rep`, and `v39-grok-4-5-high-10rep`
(10 reps per cell, final configuration). From v28
on, each version also keeps a directory with the full transcripts —
including native reasoning text — and per-run `diagnostics` code counts in
the records. The 749 archived runs cost $29.94; the sonnet-4.5
primer-iteration runs of §2.2–2.3
(v16–v18) survive only as harness logs — their result directories were lost
to a `target/` clean, which is why everything since is archived here.

The §3.3 runs depend on runlet changes that are not yet on crates.io
(`Schema::type_notation`, the enriched RL2103, `list.range` step,
`time.format` whole-second output); `agentkit-tool-compose` temporarily
points at a local `runlet` checkout carrying them.
