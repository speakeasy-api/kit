# Harness comparison: shipping issues in a production repo

Kit, Codex CLI, and Claude Code were each used to ship Linear issues in the same repository, [speakeasy-api/gram](https://github.com/speakeasy-api/gram), over July–August 2026. This page compares those sessions from their transcripts. Each session was matched to the pull request it opened (verified from the transcript's `gh pr create` result), and PR sizes were taken from `gh pr view`. All 16 PRs merged.

Because several Kit PRs are mostly generated Goa/OpenAPI code, rows are sized by **hand-written lines**: files under `server/gen/**`, `*.sql.go`, `client/sdk/**`, `openapi3.yaml`, `migrations/*.sql`, and lockfiles are excluded from the line count.

## Sessions

| harness | PR | files / +/- | hand-written lines | model | wall | active | user msgs | total input | uncached input | output | peak ctx | subagents |
|---|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Claude Code | #4694 | 5 / +77/-18 | 87 | fable-5 | 44m | 44m | 5 | 14.0M | 357k | 46k | 215k | 3 |
| Claude Code | #5128 | 2 / +74/-54 | 128 | fable-5 | 89m | 53m | 11 | 17.5M | 162k | 38k | 184k | 0 |
| Kit | #5644 | 15 / +255/-38 | 281 | gpt-5.6-sol | 91m | 62m | 4 | 14.3M | 245k | 48k | 161k | 0 |
| Claude Code | #4987, #4997 | 8 / +215/-100 | 315 | fable-5 | 11.8h | 2.4h | 32 | 37.0M | 578k | 151k | 241k | 3 |
| Codex CLI | #3116 (2 sessions) | 17 / +338/-35 | 339 | gpt-5.5 | 99m | 80m | 18 | 34.4M | 1.86M | 79k | 233k | 0 |
| Codex CLI | #3901 | 4 / +344/-19 | 363 | gpt-5.5 | 3.8h | 2.3h | 10 | 45.4M | 2.03M | 149k | 241k | 3 |
| Kit | #5627 | 28 / +2363/-34 | 376 | gpt-5.6-sol | 69m | 53m | 1 | 15.7M | 344k | 57k | 162k | 1 |
| Claude Code | #5445 | 17 / +462/-98 | 560 | opus-5 | 3.9h | 93m | 9 | 45.7M | 1.02M | 96k | 355k | 4 |
| Kit | #5601 | 31 / +2930/-308 | 911 | gpt-5.6-sol | 2.5h | 2.0h | 5 | 57.2M | 5.24M | 298k | 266k | 34 |
| Kit | #5515 | 33 / +852/-323 | 930 | gpt-5.6-sol | 2.6h | 81m | 6 | 46.2M | 3.47M | 255k | 274k | 20 |
| Claude Code | #4855 | 18 / +1101/-60 | 1161 | fable-5 | 2.7h | 120m | 18 | 53.9M | 835k | 146k | 367k | 6 |
| Kit | #5460 | 43 / +14334/-3503 | 1791 | gpt-5.6-sol | 17.3h | 3.5h | 16 | 67.1M | 7.29M | 368k | 349k | 34 |
| Claude Code | #5230, #5229 | 45 / +4680/-308 | 1961 | fable-5 | 2.0h | 108m | 15 | 101.9M | 954k | 175k | 490k | 4 |
| Claude Code | #5827 | 20 / +1015/-1115 | 2130 | fable-5 | 61m | 61m | 1 | 50.8M | 637k | 115k | 379k | 3 |

*Active* time sums the gaps between consecutive transcript events, capping each gap at five minutes, so overnight idle is excluded. *Uncached input* is fresh input plus cache writes. *Subagents* are rolled into the parent's token totals.

## Per hand-written line

The 300–1000-line band is the only one in which all three harnesses appear.

| median per hand-written line | Kit (n=3) | Codex CLI (n=2) | Claude Code (n=2) |
|---|---:|---:|---:|
| total input tokens | **49.7k** | 113k | 99.6k |
| output tokens | 274 | 321 | 325 |
| active minutes | **0.13** | 0.31 | 0.31 |

Across all fourteen rows the ratios hold: Kit medians 49.7k input tokens and 0.13 active minutes per line, Codex CLI 113k and 0.31, Claude Code 81.5k and 0.17. Output tokens per line are similar for all three.

## Steering

User messages per session show how much intervention each run needed. Kit #5627 and Claude Code #5827 shipped from a single message. Kit's median is 5 messages; Codex CLI's is 14 (#3116 took 18 across two sessions); Claude Code's is 11 (#4987 took 32).

## Reading these numbers

- **Kit ships a hand-written line for roughly half the input tokens and half the active time** of either Codex CLI or Claude Code, with comparable output per line and the least steering.
- **Kit does it with a smaller context window** (272k for gpt-5.6-sol) and lower peak context (median 266k vs 237k / 355k), through smaller requests and delegation: three Kit runs used 20–34 subagents.
- **Tokens are not cost.** Models, tokenizers, and cache pricing differ (Anthropic bills cache writes at 1.25× and reads at 0.1×; OpenAI bills cached input at 0.1–0.25×), so no dollar figure is derived here.
- **n is small** (5 Kit, 2 Codex CLI, 7 Claude Code) and models differ per harness (gpt-5.6-sol vs gpt-5.5 vs fable-5/opus-5). Treat the ratios as indicative.
- **Quality is not measured** beyond "merged". Review effort and post-merge fixes are not in the data.

## Broader corpus

The same parsing over every session on the machine (144 Kit, 374 Codex CLI, 344 Claude Code top-level sessions plus subagents, October 2025 to August 2026) shows the same shape at lower confidence, since tasks are not matched: median input tokens per API request 41k for Kit vs 64k and 85k; median peak context 70k vs 96k vs 120k; 36% of Kit sessions use subagents vs 2% and 18%. Long sessions are routine for all three: the longest Kit session by wall-clock ran 68 hours with 11 automatic compactions.
