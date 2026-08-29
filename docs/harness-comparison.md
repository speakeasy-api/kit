# Harness comparison: Kit vs Codex CLI vs Claude Code

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

Total spend is the same order of magnitude for all three. Kit's peak context stayed lowest despite the smallest window (272k vs 258k/1M) and by far the most subagents; it spent more output tokens (subagent reasoning included).

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

Kit sustains multi-hour agentic work through many compactions, but it is not the longest-running harness in this corpus; Codex and Claude Code both have longer single sessions.

## Findings

1. **Kit sends smaller requests.** Median context per API request is 41k under Kit vs 64k (Codex) and 85k (Claude Code); median peak context 70k vs 96k vs 120k. Consistent with `compose` batching several actions per round trip and with Kit delegating into many short subagent sessions.
2. **Per-task spend is comparable.** On the ship-an-issue family all three land near 45M input tokens. Kit is not cheaper per task; it spends the budget in smaller, more parallel requests and more output.
3. **Kit uses subagents routinely** — 36% of sessions vs 2% / 18% — and nests two levels deep. Observed concurrency under one parent is modest (6) compared with Claude Code's parallel `Agent` calls (14).
4. **Compaction rate per token is similar for Kit and Codex** (5.2 vs 4.9 per 100M input) and much lower for Claude Code, mostly because of 1M-context sessions.
5. **Shepherding a PR is where Kit is lightest** (1–2 turns, 0.2–5.8M tokens), but n is tiny.

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
