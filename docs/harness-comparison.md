# Harness comparison: Kit vs Codex CLI vs Claude Code (preliminary)

> Preliminary. Medians hide the long tail (the largest single request on record is 373k tokens for Kit and 708k for Claude Code), "active time" caps idle gaps at five minutes so a 12-hour session can show as a few hours, and token totals are not cost. Treat this as a method and a first data set, not a result.

Observational numbers from the session transcripts of one developer who used all three tools for day-to-day work, parsed on 2026-08-29 with the scripts in [`scripts/harness-comparison/`](../scripts/harness-comparison/). This is not a benchmark: models, tasks, and time periods differ. Read the [caveats](#caveats) before quoting anything.

## Corpora

| Harness | Source | Top-level sessions | Subagent sessions | Models | Period |
| --- | --- | ---: | ---: | --- | --- |
| Kit | `~/.kit/sessions/*.jsonl` | 144 | 1,031 | gpt-5.6-sol (142/144) | 2026-08-18 → 2026-08-28, Kit 0.1.27–0.1.7x |
| Codex CLI | `~/.codex/sessions/**/*.jsonl` | 374 | 101 | gpt-5.6-sol, 5.5, 5.4, 5.3-codex | 2025-10 → 2026-08 |
| Claude Code | `~/.claude/projects/**/*.jsonl` | 344 | 393 | claude-fable-5, opus-5 (some 1M context) | 2026-07 → 2026-08 |

Kit sessions are dominated by work on the Kit repository itself; the other two corpora span months of unrelated projects.

## Aggregate medians (top-level sessions, linked subagents rolled in)

| metric | Kit | Codex CLI | Claude Code |
| --- | ---: | ---: | ---: |
| assistant turns | 3 | 2 | 2 |
| API requests | 37 | 34 | 24 |
| tool calls | 33.5 (compose) | 45.5 | 28.5 |
| tool calls / turn | 8.5 | 19.0 | 8.9 |
| total input tokens / session | 1.50M | 2.11M | 2.30M |
| cache-read share of input | 91% | 92% | 95% |
| output tokens / session | 16k | 14k | 19k |
| input tokens / assistant turn | 460k | 927k | 696k |
| **input tokens / API request** | **41k** | **64k** | **85k** |
| **peak context** | **70k** | **96k** | **120k** |
| wall-clock | 34m | 9m | 7m |
| active time (gaps capped at 5m) | 18m | 9m | 7m |
| max peak context in any session | 373k | 320k | 708k |
| sessions with peak context ≥ 250k | 14 | 5 | 40 |
| sessions with ≥1 compaction | 21% | 19% | 4% |
| compactions / 100M input tokens | 5.2 | 4.9 | 0.4 |
| sessions using subagents | 36% | 2% | 18% |
| max subagents in one session tree | 36 | 27 | 75 |
| max concurrent subagents under one parent | 6 | 4 | 14 |
| max subagent nesting depth | 2 | 2 | 1 |

Kit compose calls wrapped ~1.65 inner tool calls each (30,589 compose calls, 50,483 inner calls). Kit subagent sessions (n=1,029): median 16 API requests, 742k input tokens, 5 minutes active, 73k peak context; 379 at depth 1 and 650 at depth 2.

## Matched task families

Same repository and same workflow (a shared skill or slash command), different issue or PR numbers. Durations are active time.

### Ship a Linear issue end-to-end (worktree → implement → PR → CI loop)

| harness | n | median active | median turns | median tool calls | median input | median output | median peak ctx | median compactions | subagents / run |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Kit | 5 | 81m | 8 | 434 | 46.2M | 255k | 266k | 3 | 0–34 |
| Codex CLI | 1 | 2.3h | 4 | 696 | 45.4M | 149k | 241k | 3 | 3 |
| Claude Code | 4 | 90m | 12 | 304 | 43.9M | 130k | 304k | 0 | 3–6 |

Total input tokens are the same order of magnitude for all three; this is not a cost comparison. Kit's peak context stayed lowest despite the smallest window (272k vs 258k/1M) and by far the most subagents; it spent more output tokens (subagent reasoning included).

### Shepherd an existing PR to merge

| harness | n | median active | median turns | median tool calls | median input | median output | median peak ctx |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Kit | 3 | 15m | 1 | 52 | 3.0M | 16k | 148k |
| Codex CLI | 3 | 24m | 1 | 20 | 0.8M | 5k | 57k |
| Claude Code | 6 | 29m | 10 | 66 | 5.9M | 24k | 125k |

### Review uncommitted changes in the Kit repository

| harness | active | turns | tool calls | input | output | peak ctx |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Kit | 1m | 2 | 2 (+3 inner) | 38k | 1k | 9k |
| Codex CLI | 21m | 4 | 31 | 3.00M | 15k | 132k |
| Claude Code (2 runs) | 8–10m | 2–5 | 16–27 | 1.1–2.4M | 26–29k | 93–132k |

The Kit run answered a narrower question ("summarize") than the other two ("review"); treat it as illustrative only.

## Matched by PR size: shipping an issue in speakeasy-api/gram

The most controlled subset: top-level sessions in the `speakeasy-api/gram` repository whose prompt asked to ship an issue or raise a PR, and which demonstrably opened a PR (verified from each transcript's `gh pr create` result). PR size from `gh pr view`. Because three of the five Kit PRs are 72–90% generated code (Goa/OpenAPI output), rows are sized by **hand-written lines** (files under `server/gen/**`, `*.sql.go`, `client/sdk/**`, `openapi3.yaml`, `migrations/*.sql`, and lockfiles excluded). All 16 PRs merged. Produced by `scripts/harness-comparison/gram_compare.py`.

| harness | PR | files / +/- | hand-written lines | model | wall | active | user msgs | API req | total input | uncached input | output | peak ctx | compactions | subagents |
|---|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| claude | #4694 | 5 / +77/-18 | 87 | fable-5 | 44m | 44m | 5 | 104 | 14.0M | 357k | 46k | 215k | 0 | 3 |
| claude | #5128 | 2 / +74/-54 | 128 | fable-5 | 89m | 53m | 11 | 124 | 17.5M | 162k | 38k | 184k | 0 | 0 |
| kit | #5644 | 15 / +255/-38 | 281 | gpt-5.6-sol | 91m | 62m | 4 | 143 | 14.3M | 245k | 48k | 161k | 0 | 0 |
| claude | #4987, #4997 | 8 / +215/-100 | 315 | fable-5 | 11.8h | 2.4h | 32 | 283 | 37.0M | 578k | 151k | 241k | 1 | 3 |
| codex | #3116 (2 sessions) | 17 / +338/-35 | 339 | gpt-5.5 | 99m | 80m | 18 | 245 | 34.4M | 1.86M | 79k | 233k | 1 | 0 |
| codex | #3901 | 4 / +344/-19 | 363 | gpt-5.5 | 3.8h | 2.3h | 10 | 380 | 45.4M | 2.03M | 149k | 241k | 3 | 3 |
| kit | #5627 | 28 / +2363/-34 | 376 | gpt-5.6-sol | 69m | 53m | 1 | 162 | 15.7M | 344k | 57k | 162k | 0 | 1 |
| claude | #5445 | 17 / +462/-98 | 560 | opus-5 | 3.9h | 93m | 9 | 247 | 45.7M | 1.02M | 96k | 355k | 0 | 4 |
| kit | #5601 | 31 / +2930/-308 | 911 | gpt-5.6-sol | 2.5h | 2.0h | 5 | 668 | 57.2M | 5.24M | 298k | 266k | 3 | 34 |
| kit | #5515 | 33 / +852/-323 | 930 | gpt-5.6-sol | 2.6h | 81m | 6 | 497 | 46.2M | 3.47M | 255k | 274k | 4 | 20 |
| claude | #4855 | 18 / +1101/-60 | 1161 | fable-5 | 2.7h | 120m | 18 | 315 | 53.9M | 835k | 146k | 367k | 0 | 6 |
| kit | #5460 | 43 / +14334/-3503 | 1791 | gpt-5.6-sol | 17.3h | 3.5h | 16 | 809 | 67.1M | 7.29M | 368k | 349k | 11 | 34 |
| claude | #5230, #5229 | 45 / +4680/-308 | 1961 | fable-5 | 2.0h | 108m | 15 | 379 | 101.9M | 954k | 175k | 490k | 1 | 4 |
| claude | #5827 | 20 / +1015/-1115 | 2130 | fable-5 | 61m | 61m | 1 | 228 | 50.8M | 637k | 115k | 379k | 0 | 3 |

"Uncached input" is fresh input plus cache writes; Claude's raw fresh input is near zero in every session because everything is a cache read or cache write, so it is the only comparable column. Rows sized 300–1000 hand-written lines are the only bucket with all three harnesses:

| per hand-written line, median (300–1000 bucket) | Kit (n=3) | Codex (n=2) | Claude (n=2) |
|---|---:|---:|---:|
| total input tokens | **49.7k** | 113k | 99.6k |
| uncached input tokens | 3.7k | 5.5k | **1.8k** |
| output tokens | 274 | 321 | 325 |
| active minutes | **0.13** | 0.31 | 0.31 |
| API requests per session | 497 | 312 | 265 |
| peak context | 266k | 237k | 298k |
| compactions | 3 | 2 | 0 |

Across all rows the picture is the same: Kit ships a hand-written line for about half the total input tokens and half the active time of the others, at the cost of the most API requests and the most compactions; Claude Code has by far the lowest uncached input because nearly everything it sends is a cache hit.

Outcome notes:

- Two PRs needed a separate shepherd session to merge: Kit #5601 (a Claude session three days later) and Claude #5827 (a Claude session the next day). Kit #5601's row excludes that cost; the Codex and Claude rows that shepherded their own PR include the polling time (Codex #3901 3.8h wall, Claude #4987 11.8h wall).
- No issue in the set was re-implemented in another harness afterwards.
- User-message counts show how much steering each run needed: Kit #5627 and Claude #5827 were one-shot; Codex #3116 took 18 messages across two sessions, Claude #4987 took 32.

Additional caveats for this subset: n is 5 / 2 / 7; models differ per row (gpt-5.6-sol vs gpt-5.5 vs fable-5/opus-5) and tokenizers differ, so token ratios are indicative, not price-comparable (Anthropic bills cache writes at 1.25× and reads at 0.1×; OpenAI bills cached input at 0.1–0.25×); the generated-file regex is heuristic; Linear estimates are unset on every issue, so PR size is the only available size proxy.

## Longest sessions

| harness | criterion | wall / active | turns | API requests | input | peak ctx | compactions | subagents |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Kit | active time | 27.6h / 4.7h | 22 | 457 | 41.3M | 218k | 1 | 4 |
| Kit | API requests | 6.4h / 2.9h | 17 | 886 | 60.5M | 247k | 3 | 29 |
| Kit | compactions | 17.3h / 3.5h | 15 | 809 | 67.1M | 349k | 11 | 34 |
| Kit | input tokens | 20.8h / 3.3h | 19 | 694 | 76.2M | 219k | 2 | 9 |
| Codex CLI | active time | 84.5h / 42.6h | 66 | 1,529 | 188.6M | 232k | 34 | 12 |
| Claude Code | active time | 31.4h / 11.6h | 95 | 2,528 | 630.5M | 708k | 3 | 15 |
| Claude Code | API requests | 42.8h / 9.0h | 53 | 5,794 | 1,255.9M | 570k | 3 | 75 |

"Active" undercounts long sessions with waits (CI, reviews, sleeps); the longest Kit session by wall-clock is 68 hours. Kit sustains multi-hour agentic work through many compactions, and Codex and Claude Code both have longer single sessions on this data.

## Findings

1. **Kit sends smaller requests.** Median context per API request is 41k under Kit vs 64k (Codex) and 85k (Claude Code); median peak context 70k vs 96k vs 120k. Consistent with `compose` batching several actions per round trip and with Kit delegating into many short subagent sessions.
2. **Per-task input tokens are comparable.** On the ship-an-issue family all three land near 45M input tokens. Kit does not use fewer tokens per task; it uses them in smaller, more parallel requests and more output. No pricing is applied, so this says nothing about cost.
3. **Kit uses subagents routinely** — 36% of sessions vs 2% / 18% — and nests two levels deep. Observed concurrency under one parent is modest (6) compared with Claude Code's parallel `Agent` calls (14).
4. **On size-matched gram issues, Kit used roughly half the total input tokens and half the active time per hand-written line** of Codex CLI or Claude Code, with more API requests and compactions. Small n; see the matched section.
5. **Compaction rate per token is similar for Kit and Codex** (5.2 vs 4.9 per 100M input) and much lower for Claude Code, mostly because of 1M-context sessions.
6. **Shepherding a PR is where Kit is lightest** (1–2 turns, 0.2–5.8M tokens), but n is tiny.

## Caveats

- **Different models.** Kit ran gpt-5.6-sol almost exclusively; Codex mixed five models over ten months; Claude Code used Anthropic models with larger windows. Kit vs recent Codex is a same-model comparison; Kit vs Claude Code is not.
- **Different tasks and periods.** Families are workflow matches, not identical prompts. No prompt was sent verbatim to Kit and another harness.
- **Cache accounting differs.** OpenAI `input_tokens` includes cached tokens; Anthropic reports fresh, cache-read, and cache-write disjointly. "Total input" is context sent, not cost.
- **Kit rollups are lower bounds.** 420 of 1,031 Kit subagent transcripts could not be linked to a parent and are excluded from parent totals. Kit subagents that ran Codex over ACP (29 sessions) are counted under Codex.
- **Turn counting differs** per harness (Kit: completed responses; Codex: task-complete events or assistant messages; Claude Code: user prompts).
- **Wall-clock is inflated by idle time**; use the active column.
- **Kit changed during the sample** (0.1.27 → 0.1.7x): compaction, backgrounding, and subagent limits all moved.

## Reproduce

```sh
python3 scripts/harness-comparison/parse_all.py   # writes sessions.json next to the script
python3 scripts/harness-comparison/analyze.py > analysis.md
```

Standard library only; ~15 seconds on 2,655 sessions. Edit the family regexes in `analyze.py` to match your own repositories and workflows.
