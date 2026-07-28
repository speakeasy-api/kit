# compose-bench

Benchmarks the effectiveness of the `compose` Lua tool against granular
tool-calling on life-like agent tasks.

## Thesis under test

Agents that have Bash available prefer it for multi-step work because one
shell pipeline replaces many tool round-trips. `compose` claims to bring that
same benefit to tool surfaces Bash cannot reach (MCP servers, built-in tools):
one Lua script calling `tool(name, input)` N times costs one model round-trip
instead of N.

If the thesis holds, the `compose` arm should show fewer model requests, less
cumulative context, lower cost, and lower wall time than the `granular` arm at
equal-or-better accuracy — and on the file-backed scenario it should approach
the `bash` arm's numbers.

## Design

### Arms

| arm        | tool surface                                                                                                 |
| ---------- | ------------------------------------------------------------------------------------------------------------ |
| `granular` | scenario tools only                                                                                          |
| `compose`  | the same tool source wrapped by `ComposeTool::wrap` — compose **and** the granular tools are both advertised |
| `bash`     | `shell_exec` only (file-backed scenario only)                                                                |

The system prompt is identical in every arm and deliberately neutral ("be
efficient"), never mentioning compose. In the `compose` arm the model is free
to ignore compose entirely, so the _compose share_ column measures genuine
preference, not compliance.

### Scenarios

All worlds are deterministic in-memory fixtures behind mock tools; each run
gets a fresh world. Ground truth is recomputed by the scorer from the same
fixture code, and unit tests pin the interesting properties of each fixture
(`cargo test -p compose-bench`).

| scenario              | shape                                  | what makes it hard                                                                                      |
| --------------------- | -------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `support-triage`      | read + targeted writes                 | 3-field predicate; bodies only visible via `get_ticket`; distractors on every predicate axis            |
| `revenue-report`      | read-only N+1 aggregation              | amounts/statuses need `get_order`, regions need `get_customer`; refunded/pending and other-month noise  |
| `log-incident`        | read-only investigation                | sustained error burst vs background noise; correlate burst start with deploy times; owner lookup        |
| `crm-hygiene`         | write-heavy normalization              | 18 phone normalizations + 4 company backfills; already-valid records must be untouched                  |
| `calendar-scheduling` | read-only constraint solve             | 4 people x 5 days of availability reads, earliest common 60-min slot                                    |
| `config-migration`    | file-backed migration (has `bash` arm) | rename `timeout_ms` -> `request_timeout_ms` incl. nested keys; `connect_timeout_ms` is a substring trap |

Every scenario ends with a `submit_result` call carrying a structured answer,
so accuracy is scored mechanically (world-state assertions + answer checks
with partial credit), never by parsing prose.

### Metrics (per run)

- **wall time** — end-to-end seconds
- **model requests** — API round-trips (one `UsageUpdated` event per request)
- **tool calls / compose share** — top-level calls; tools invoked _inside_ a
  compose script do not re-enter the loop and are intentionally not counted
- **total tokens** — sum of input + cached input + output across all requests
  (what you pay for, modulo cache discounts)
- **peak ctx** — largest single request (input + cached + output): how full
  the context window got
- **cost** — OpenRouter-reported USD when present
- **accuracy** — 0..1 rubric per scenario

Raw runs land in `runs.jsonl`, full transcripts (including every Lua script
the model wrote) in `transcripts/`, and an aggregated markdown table with
compose-vs-granular deltas in `report.md`.

## Running

```bash
# .env at the workspace root works too (dotenvy)
export OPENROUTER_API_KEY=...
export OPENROUTER_MODEL=anthropic/claude-sonnet-4.6

cargo run -p compose-bench --release -- --reps 3
```

Useful flags:

```
--scenarios support-triage,revenue-report   subset of scenarios
--arms granular,compose                     subset of arms
--reps 5                                    repetitions per cell (default 1)
--max-requests 60                           abort a run after N model requests
--timeout-secs 600                          wall-clock cap per run
--tool-latency-ms 80                        simulated per-call MCP latency
--compose-max-nested 256                    nested-call budget inside a compose script
--out target/compose-bench-results          output directory
```

`--compose-max-nested` defaults to 256 (the crate default is 64, which is
smaller than a naive full-fan-out script for some scenarios; the benchmark
raises it so arms compare composition, not error-recovery skill).

`--tool-latency-ms` matters for the time claim: with 0 latency the granular
arm's only time penalty is model round-trips. Real MCP servers add network
latency per call, which compose amortizes into one round-trip — run with e.g.
`--tool-latency-ms 80` for the more realistic comparison. Note it applies to
mock scenario tools only, not to `fs_*`/`shell_exec` in `config-migration`.

## Caveats

- One model per invocation; compare models by running twice with different
  `OPENROUTER_MODEL`.
- Costs depend on OpenRouter including `usage.cost` in responses; when absent
  the column is blank (tokens are always recorded).
- Repetitions are sequential, not parallel, to keep provider-side caching and
  rate limits from skewing arms differently.
- `compose` nested calls don't count toward `--max-requests`; the cap bounds
  model round-trips, which is the resource under test.
