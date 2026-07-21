# RFC 0001: Kit, an Efficiency-First Coding Agent Runtime

- Status: Draft
- Date: 2026-07-21
- Authors: Kit project
- Target: Initial complete product architecture

## 1. Summary

Kit is a headless coding-agent daemon designed to move the Pareto frontier of four outcomes:

1. verified accuracy,
2. end-to-end speed,
3. token efficiency,
4. cost per verified result.

Kit is not primarily a chat client. It is the durable runtime that owns every thread, run, model turn, tool call, process, terminal, workspace, subagent, peer-agent task, approval, artifact, and compacted continuation. It exposes that state through a public server API. A thin CLI, ACP-compatible editors, a built-in observability page, and third-party clients all use the same runtime.

Kit is written in Rust and built on `agentkit`. Agentkit provides normalized model content, provider adapters, the live agent loop, capability and tool abstractions, MCP and ACP integration, concurrent foreground/background tool-task primitives, reporting, and compaction seams. Kit adds the opinionated coding product around those primitives: durable run and job scheduling, ownership, repository intelligence, optimized prompts and tool surfaces, transactional edits, verification, model routing, process isolation, inter-agent protocols, observability, and rigorous evaluation.

This RFC deliberately describes the complete intended system rather than an MVP. Features may ship incrementally, but the architecture must not make the end state impossible.

## 2. Motivation

Most coding agents spend substantial resources on work that does not improve the probability of a correct patch:

- verbose status narration and final responses,
- repeated transmission of stable prompts and tool schemas,
- broad repository reads before localization,
- one model round-trip per mechanically composable tool call,
- raw logs and intermediate values retained in context,
- whole-file rewrites for local edits,
- tests selected without regard to affected code,
- expensive models used for deterministic or low-risk work,
- repeated failed approaches after the evidence has stopped changing,
- summaries produced at arbitrary token limits rather than semantic boundaries,
- tool catalogs too large for reliable selection,
- model-facing schemas optimized for machines rather than model comprehension,
- orchestration performed by the model where a parser, compiler, graph, or program is exact.

The central design rule is:

> Spend model tokens and wall-clock time only where they materially change the probability of a verified correct result.

This is not equivalent to minimizing tokens. An extra targeted read or test can reduce total cost by preventing a bad edit and a repair turn. Kit optimizes the complete trajectory, including failures, retries, verification, infrastructure, and human intervention.

## 3. Goals

Kit MUST:

- produce correct, scoped, maintainable code changes with executable evidence;
- minimize cost per independently verified result rather than cost per attempt;
- minimize time to verified completion, including queueing, setup, tools, builds, and retries;
- account separately for uncached input, cache writes, cache reads, visible output, reasoning tokens, tools, compute, and failed speculation;
- make stable prompt prefixes and dynamic context intentional and observable;
- discover relevant code with lexical, syntactic, semantic, graph, and historical evidence under explicit token budgets;
- expose concise, model-legible tools that can batch work and return bounded output;
- apply edits transactionally against a known workspace revision and optionally verify them in the same call;
- discover large and changing tool catalogs without loading every schema into context;
- compose tools, including MCP tools, while retaining normal permission, tracing, cancellation, and resource controls;
- support ACP subagents and clients, A2A peer collaboration, and MCP capabilities without conflating their roles;
- compact at model-selected semantic boundaries while retaining lossless external history;
- use compact type projections and adaptive TOON where they measurably help;
- own and supervise all child processes and agents;
- expose one public API with CLI parity and resumable event streams;
- remain useful without a sophisticated first-party UI;
- evaluate every optimization with reproducible, statistically credible experiments.

## 4. Non-Goals

Kit is not:

- a general IDE or a best-in-class graphical client;
- a replacement for `agentkit` provider and loop abstractions;
- a fork of ACP, A2A, MCP, JSON Schema, LSP, SCIP, or TOON;
- a promise that model output can be replayed bit-for-bit;
- a promise that cancellation rolls back external effects;
- a claim that one model, edit format, composition language, or retrieval strategy is optimal for every task;
- a system that persists or exposes hidden chain-of-thought;
- a universal mutable AST API;
- a benchmark-specialized SWE-bench harness presented as a general coding agent.

## 5. Normative Language

The terms MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are normative. “Initial version” means the first complete product described by this RFC, not the first executable milestone.

### 5.1 Requirement tracking

Before implementation begins or this RFC advances beyond Draft, every normative statement MUST receive a stable identifier of the form `KIT-<AREA>-NNN`, and every identifier MUST map to one or more of: a conformance test, an evaluation, an operational assertion, or an explicitly documented manual review. IDs remain stable when text moves; retired requirements are tombstoned rather than reused. CI rejects unregistered normative statements and reports requirement-to-evidence coverage. Delivery acceptance criteria in §34 reference these requirement sets rather than treating prose completion as implementation completion.

## 6. Success Definition

### 6.1 Primary outcomes

Kit has no permanent scalar “agent score.” Correctness and safety are gates. Eligible configurations are compared on a Pareto frontier.

For a predeclared task population, let `C_i` be the fully loaded cost of the complete product-policy trajectory for task `i`, including failed attempts and verification, and let `Y_i` be one only when the fixed external acceptance rule accepts the final result. The primary production efficiency metric is the ratio of totals:

```text
cost per verified result = sum(C_i) / sum(Y_i)
```

The ratio is undefined when no result is accepted. Reports include repository/task-clustered confidence intervals and never average per-task `C_i / Y_i` values or compare ratios across different task mixes.

“External verification” means the acceptance rule is fixed before the run, cannot be read or modified by the agent, and executes outside the agent’s authority boundary. This does not imply that a grader is complete or independent in provenance; grader provenance, mutation strength, and known false-positive/false-negative risks are reported separately.

The primary latency view is a time-to-verified-completion curve with unsuccessful runs still incomplete at the trial budget. Conditional p50/p95 among successful runs is reported only alongside unconditional completion probability by fixed deadlines and the full cost of failures. The primary accuracy metric is externally graded task resolution including regression checks. The primary reliability metric is repeated-run consistency.

Every benchmark or optimization result MUST report at least:

- resolution rate and confidence interval,
- `FAIL_TO_PASS` and `PASS_TO_PASS` outcomes where applicable,
- regression, security, and unauthorized-effect rate,
- p50/p95 time to first useful action, first edit, targeted-test pass, and verified completion,
- logical and billed tokens by category,
- fully loaded cost per attempt and per verified result,
- model turns, tool calls, nested tool calls, and repair loops,
- cache hit/write/read ratios and prefix divergence,
- human intervention, review, rework, and rollback.

Operational reporting is tiered. Every run records a core envelope containing outcome, total latency, model/tool usage and cost, cache categories when available, checks, and errors. Benchmark/experiment reports add the complete metric families above and uncertainty. Production reports add delayed human review, rework, rollback, and defect outcomes. A field that is unavailable is marked unavailable rather than estimated silently.

### 6.2 Optimization acceptance

An optimization ships only if it:

1. improves correctness without violating safety, cost, or latency limits;
2. reduces cost or latency while remaining inside a predeclared correctness non-inferiority margin; or
3. reduces operational or security risk at an accepted incremental cost.

Token reduction without task-level evidence is not success.

## 7. Product Principles

### 7.1 Deterministic before generative

Kit SHOULD use parsers, compilers, formatters, codemods, hashes, search indexes, dependency graphs, version control, and tests before asking a model to infer facts those systems can determine exactly.

### 7.2 Progressive disclosure

Kit SHOULD expose identifiers and summaries first, focused source ranges second, full files third, and broad neighborhoods only after evidence requires them.

### 7.3 Smallest sufficient interaction

Prompts, schemas, results, edits, and final answers SHOULD contain the minimum information needed for the next correct decision. “Terse” never means omitting requirements or evidence.

### 7.4 Verify effects, not prose

Completion is established by repository state and independent checks, not by the model saying a task is complete.

### 7.5 One authority per concern

- Agentkit owns the active model/tool loop.
- Kit owns durable lifecycle and product policy.
- The workspace revision owns file truth.
- The capability broker owns authorization and invocation.
- Protocol adapters own wire conversion, not domain state.
- The event log owns what happened.

### 7.6 Model-specific, evidence-driven policy

Tool descriptions, prompt modules, edit formats, reasoning levels, compaction policy, composition backends, and result encodings MAY vary by model. Defaults MUST come from measured behavior, not aesthetic preference.

### 7.7 Lossless outside, compact inside

Kit stores complete authorized events and artifacts outside model context. Model context is a revocable, compact view over that state. Compaction never destroys the source record.

## 8. System Architecture

```text
                   ACP editors       CLI       Web/SDK clients
                        |              |              |
                        +------- public API ----------+
                                       |
                              command/query service
                                       |
                 +---------------------+---------------------+
                 |                     |                     |
            event store          run scheduler        artifact store
                 |                     |                     |
                 +------------- run supervisor --------------+
                                       |
                +----------------------+----------------------+
                |                      |                      |
          agentkit loop        capability broker      workspace service
                |                      |                      |
       model/provider router    native/MCP/compose     search/edit/verify
                |                      |                      |
                +---------------- sandbox executor -----------+
                                       |
                        processes, LSPs, tests, ACP agents

                A2A gateway <---- durable remote tasks ----> peers
```

Kit SHOULD initially ship as one binary with modular internal boundaries. It MAY split control-plane and executor services later without changing public semantics.

### 8.1 Required modules

- `api`: HTTP, SSE, WebSocket terminal transport, OpenAPI, authentication.
- `cli`: thin client over the public command/query contract.
- `domain`: IDs, commands, events, projections, lifecycle state machines.
- `store`: embedded and service persistence implementations.
- `runtime`: scheduling, leases, supervision, cancellation, backpressure.
- `agent`: agentkit assembly, prompt compiler, model router, transcript projection.
- `capabilities`: native tools, MCP broker, discovery, permissions, composition.
- `workspace`: snapshots, filesystem index, code intelligence, edits, diffs.
- `verify`: diagnostics, affected-test selection, builds, tests, evidence.
- `protocols`: ACP client/server, A2A gateway, MCP client/server adapters.
- `executor`: local, restricted container, and isolated VM backends.
- `telemetry`: events, traces, metrics, usage, cost, experiment attribution.
- `web`: optional static observability application using only the public API.

## 9. Relationship to Agentkit

Kit initially targets the reviewed local `agentkit` source tree derived from release `0.10.2`. Reproducible builds MUST pin its commit or tree digest, Cargo lockfile, enabled feature set, and exact Runlet dependency revision. Runlet and TOON compose support are experimental, optional features in the reviewed tree, not guarantees inferred from the semver alone.

Agentkit already provides:

- `agentkit-core`: normalized `Item`, `Part`, `Delta`, identifiers, usage, and cancellation primitives;
- `agentkit-loop`: `Agent`, `LoopDriver`, model sessions, tool round-trips, interrupts, observers, and mutators;
- `agentkit-capabilities`: invocables, resources, and prompts;
- `agentkit-tools-core`: tool registry, execution, permissions, and approvals;
- `agentkit-task-manager`: foreground, parallel, and detached tool tasks;
- `agentkit-context`: `AGENTS.md` and context loading;
- `agentkit-compaction`: structural and semantic transcript mutation;
- `agentkit-mcp`: MCP lifecycle, discovery, tools, resources, prompts, auth, and server events;
- `agentkit-acp`: ACP session integration and headless serving;
- `agentkit-tool-compose`: Lua and optional Runlet composition with optional TOON results;
- model adapters and reporting hooks.

Kit MUST NOT turn agentkit’s in-memory loop into the durable source of truth. One active run attempt owns one `LoopDriver`. Kit reconstructs its transcript from committed events and snapshots and resolves blocking interrupts through durable approvals and auth state.

Agentkit observers are infallible observation hooks, so they are telemetry surfaces rather than durable effect boundaries. Kit-owned `ModelAdapter`, `ToolExecutor`, mutator, and capability wrappers MUST commit intent before dispatch, then commit outcome before returning to the loop.

The reviewed agentkit loop validates transcript invariants only after mutators return. Kit therefore requires an agentkit post-validation, fallible checkpoint hook before semantic compaction is enabled. A mutator writes only a candidate artifact; the hook atomically promotes the validated final transcript/checkpoint before the next model call. Until that hook exists, Kit may perform only structural mutations whose complete invariants it independently duplicates and versions.

The reviewed loop cannot reconstruct arbitrary in-flight provider sessions, tools, detached tasks, approvals, or auth operations from a transcript alone. Restart is therefore allowed only from persisted safe boundaries. An interrupted in-flight operation becomes a durable error result or `outcome_unknown` before a fresh `LoopDriver` is constructed, unless agentkit later gains a compatible durable-resume contract.

Agentkit’s task manager is suitable for tool work within a live turn. Durable subagents and independently resumable jobs are Kit `Run` entities.

Kit SHOULD upstream reusable improvements to agentkit when they are product-neutral. Kit-specific repository policy, persistence, routing, and public API remain in Kit.

## 10. Durable Domain Model

### 10.1 Entities

| Entity | Purpose |
| --- | --- |
| `Principal` | User, service, or agent identity and policy subject |
| `Project` | Repository configuration, prompt policy, indexes, and eval attribution |
| `Thread` | Durable user-visible conversation and task history |
| `Run` | One requested agent execution |
| `Attempt` | One lease-bound execution of a run |
| `Turn` | One user-visible prompt-to-yield interval |
| `ModelCall` | One provider inference request |
| `ToolCall` | One direct or nested capability invocation |
| `Task` | Background tool/process work owned by a run |
| `AgentLink` | ACP subagent or A2A peer relationship |
| `ExternalTask` | Durable A2A task/message exchange and remote lifecycle |
| `DaemonService` | Scoped owner for long-lived LSP, MCP, connection, and indexing processes |
| `Workspace` | Revisioned mutable checkout or immutable snapshot |
| `Process` | OS or sandbox process owned by an attempt or daemon service |
| `Terminal` | Optional PTY attached to a process |
| `Approval` | Durable human or policy decision |
| `Checkpoint` | Model-selected semantic compaction boundary |
| `Artifact` | Content-addressed log, diff, file, index, image, or report |
| `Experiment` | Configuration assignment and measured outcome |

### 10.2 Run lifecycle

```text
queued -> acquiring_workspace -> starting -> running
running -> waiting_for_input | waiting_for_approval | waiting_for_auth
waiting_for_input | waiting_for_approval | waiting_for_auth -> running
running -> completed | failed | cancelling | interrupted
cancelling -> cancelled | failed | interrupted
interrupted -> queued (new attempt) | failed
any nonterminal state -> cancelling
```

An attempt has `leased -> executing -> quiescing -> succeeded|failed|interrupted`, with lease loss forcing `quiescing` before another attempt may acquire ownership. State commands use compare-and-set expected versions. Waiting runs may release model capacity but retain their durable lease/resource policy. Terminal states are monotonic. Retries create a new `Attempt`; they do not revive an old one.

### 10.3 Ownership invariants

1. Every process has exactly one `Attempt` or scoped `DaemonService` owner; every terminal, workspace writer, model call, and local child agent has exactly one owning run attempt.
2. A workspace has at most one mutable writer unless an explicit merge operation is active.
3. A stale attempt cannot commit after losing its lease.
4. Every externally visible side effect has a durable intent and outcome, or an explicit `outcome_unknown`.
5. Cancellation propagates downward but never implies rollback.
6. Historical replay never re-executes an external side effect.
7. Semantic events are not dropped because a client is slow.
8. All queues, output buffers, process counts, subagent counts, and spend are bounded.

Long-lived LSPs, MCP servers, and shared connections are daemon-service processes scoped to a principal, project, workspace revision policy, and resource budget. Remote ACP endpoints and A2A peers are owned as durable task relationships; Kit does not claim ownership of their OS processes.

### 10.4 Event storage

Kit uses an append-only semantic event log with current-state projections. Large or high-volume content is stored as content-addressed artifacts and referenced by hash.

The default local daemon SHOULD use embedded SQLite in WAL mode plus a local artifact directory so installation remains one binary and one state root. It requires a process-wide state-root lock, local-disk storage, bounded busy handling, migrations, and startup orphan reconciliation. A PostgreSQL/object-store implementation MAY support clustered or multi-tenant deployment.

Both stores MUST atomically append one command’s events across affected streams, check expected stream versions, update projections and the idempotency record, assign a store-wide monotonic `commit_position`, and increment monotonic fencing counters. Lease acquisition and expiry use authoritative store time; fencing tokens never derive from timestamps. `(stream, sequence)` and `commit_position` are unique. Commit positions are assigned under a commit-serialization lock and published only after commit, or readers use a committed-prefix watermark they cannot advance beyond; a later visible transaction can never make a client skip an earlier one. Event streams resume from an opaque committed-position cursor, not a stream-local sequence.

Artifacts are written, synced or upload-confirmed, and hash-verified before events reference them. Unreferenced uploads are garbage-collected; referenced artifacts follow declared event retention. If online history expires, streaming returns a cursor-expired response with a current projection snapshot and new cursor rather than silently skipping events.

The local store MUST create periodic consistent SQLite backup snapshots plus artifact manifests, verify restoration automatically, retain multiple generations outside the active state root, and expose last-successful-backup health. WAL checkpointing is operational maintenance, not a backup. Remote stores require an equivalent tested point-in-time recovery policy.

Representative event envelope:

```json
{
  "id": "evt_...",
  "stream": "run_...",
  "sequence": 42,
  "commit_position": 1739,
  "type": "tool_call.completed",
  "schema_version": 1,
  "occurred_at": "2026-07-21T12:00:00Z",
  "causation_id": "cmd_...",
  "correlation_id": "thread_...",
  "attempt_id": "attempt_...",
  "trace_id": "...",
  "payload": {},
  "artifacts": ["blake3:..."]
}
```

Streamed token and terminal deltas MAY be coalesced into bounded artifact chunks. Committed transcript items, tool intents/results, approvals, edits, verification outcomes, checkpoints, costs, and lifecycle transitions are semantic events.

Kit MUST NOT persist hidden chain-of-thought. Provider-returned reasoning summaries are stored only under explicit retention policy.

## 11. Prompt System

### 11.1 Prompt compiler

The system prompt is compiled from independently versioned modules:

1. immutable safety and authority rules,
2. concise operating behavior,
3. coding and testing quality rules,
4. tool routing rules,
5. repository instructions,
6. task requirements and acceptance criteria,
7. retrieved evidence and active continuation state.

Stable modules MUST precede dynamic modules to maximize exact-prefix caching. Serialization, whitespace, tool order, and key order MUST be canonical. Timestamps and run-specific IDs MUST NOT appear in the stable prefix.

Every standing instruction requires one of:

- a safety or product requirement,
- an observed recurrent model failure,
- a measured improvement in an ablation.

Kit maintains a prompt deletion suite: instructions are periodically removed and retained only when their value remains measurable.

### 11.2 Default behavioral policy

The measured core policy MUST be compiled by default and encode the following in the shortest model-effective wording. Evaluated model-specific variants MAY remove or rewrite rules but cannot weaken security, authority, or workspace-safety requirements:

- act on the task rather than describing a proposed solution;
- inspect relevant code before editing;
- communicate only discoveries, decisions, blockers, and final evidence;
- do not narrate routine tool calls or restate the user’s request;
- prefer the smallest correct change and existing abstractions;
- do not add compatibility paths, helpers, dependencies, or configuration without a concrete need;
- parallelize independent discovery and checks;
- continue through implementation and verification unless genuinely blocked;
- write comments only for non-obvious intent, never as a narration of code;
- name tests after enduring behavior, not the bug, ticket, or recent implementation change;
- test externally observable behavior rather than mirroring implementation steps;
- preserve unrelated work and never discard uncommitted changes without explicit authority;
- use executable evidence, not confidence, to claim completion;
- return a concise outcome, changed areas, and checks run;
- do not reveal private reasoning; provide decisions and evidence when explanation is needed.

These are behavior constraints, not a mandatory verbose workflow. Models that already satisfy them should pay little prompt overhead.

### 11.3 Task normalization

Before execution, Kit builds a compact task contract:

```text
goal
explicit requirements
inferred acceptance criteria, marked as inferred
scope and protected areas
available verification
risk class
resource budget
```

The model SHOULD ask a question only when uncertainty materially changes implementation or safety. Otherwise it proceeds and records assumptions.

### 11.4 Output control

Kit SHOULD avoid requesting visible chain-of-thought. It requests concise decisions, evidence, patches, or structured control values and tells the model not to over-analyze routine steps. Provider reasoning effort and maximum reasoning tokens are routed separately from visible verbosity; Kit measures whether “think tersely” steering reduces hidden output without hurting verified accuracy.

Machine-consumed responses SHOULD use constrained structured output or tool calls. Human-facing prose remains plain text and terse.

## 12. Context Engineering and Caching

### 12.1 Context layers

The active model view is assembled in this order:

```text
stable policy and compact tool signatures
repository-level instructions and stable map
checkpoint continuation packet
current task contract
recent transcript items
retrieved code/evidence
latest tool-result deltas
```

Requirements, active failures, changed files, and unresolved decisions outrank old raw tool output. Large artifacts are represented by handles with small previews and can be read on demand.

### 12.2 Prefix caching

Kit records, per model request:

- canonical prompt template version,
- stable-prefix digest,
- first divergence token or byte,
- uncached, cache-write, and cache-read tokens,
- retention policy and provider cache key,
- time to first token and prefill duration when available.

Cache-write tokens, provider cache keys, residency, and prefill duration are recorded only when exposed by the provider; unavailable values remain null and estimates are labeled as estimates.

Tool and prompt changes are batched where practical because a token difference prevents reuse of the previously cached prefix beyond the first divergence for that request; the matching earlier prefix and other cache entries may remain reusable. Parallel workers MAY warm a shared stable prefix before fan-out when provider semantics make that beneficial.

Provider-side conversation IDs are not treated as proof of KV-cache residency, billing discounts, or durable state.

### 12.3 Context value accounting

Every inserted context block carries provenance, revision, retrieval reason, estimated tokens, and optional relevance score. This enables offline attribution: which evidence was used, which was ignored, and which missing evidence caused failure.

Kit SHOULD learn token budgets by task and model. It MUST retain a deterministic fallback budget.

## 13. Repository Intelligence

### 13.1 Layered discovery

Kit uses a progressive hybrid stack:

1. `.gitignore`-aware file metadata and exact lexical search,
2. incremental Tree-sitter syntax trees and language queries,
3. `ast-grep` structural search and previewable rewrites,
4. long-lived LSP servers for live semantic answers and diagnostics,
5. SCIP indexes for persistent compiler-derived symbol identity where available,
6. bounded code-graph traversal,
7. optional sparse and dense semantic retrieval,
8. version-control history, blame, and changed-together signals.

Lexical search remains the universal fast path and fallback. Embeddings are an optional recall source, never the only index.

Layers 5-7 (SCIP, the derived code graph, and sparse/dense semantic retrieval) are optional adapters selected by repository language, scale, available indexes, and measured localization value. Kit remains conformant and dogfoodable without them; lexical search, Tree-sitter, structural search, and available LSP semantics form the core discovery path.

### 13.2 Canonical indexed unit

Symbol records SHOULD contain:

```text
repository revision
path and language
qualified and display names
kind and signature
source byte range
enclosing symbol
imports and exports
definitions and references
callers/callees where known
associated tests
documentation excerpt
source and confidence of each fact
```

Facts from Tree-sitter are syntactic. Facts from LSP or SCIP are semantic. Kit MUST preserve that distinction rather than silently treating same-spelling identifiers as resolved references.

### 13.3 Code graph

The graph contains repositories, packages, files, symbols, tests, diagnostics, and revisions with edges such as:

```text
contains, defines, imports, exports, references, calls,
implements, inherits, overrides, tests, changed_with
```

Each edge includes provenance, confidence, revision, and range. Traversal is purpose-specific, hop-bounded, degree-penalized, and token-budgeted. Kit does not attempt to build a universal program representation.

### 13.4 Personalized repository map

For each task, Kit ranks declarations using task terms, exact identifiers, stack traces, recently read files, current edits, semantic edges, test relationships, and graph centrality. It returns signatures and a few high-value lines under a token budget.

The map is not injected unchanged on every turn. It is generated or updated when the task neighborhood changes.

### 13.5 Targeted tree output

The `discover` call accepts task/query terms, roots, languages, relationship filters, expansion cursor, and a token or item budget. It returns the workspace revision, ranked entries, short ranking reasons, omitted counts, and cursors scoped to the same revision. It MUST default to a terse, ranked view rather than an exhaustive dump. Example:

```text
src/
  auth/
    service.rs:12-148  AuthService, login, refresh       [task 0.94, refs 17]
    token.rs:8-93      Claims, verify                    [task 0.88, test 3]
  api/session.rs:21-77 create_session                    [caller]
tests/auth.rs:14-120   login_expiry, refresh_rotation    [covers 2 symbols]
Cargo.toml                                                   [dependency]
... 143 files omitted; expand(path|symbol|edge)
```

The model can expand a path, symbol, relationship, or score band. Generated, vendored, binary, and minified content is classified and suppressed by default.

### 13.6 Search contract

One batched search call SHOULD support multiple independent queries and modes:

```text
search(
  queries: [{text, mode: lexical|structural|symbol|semantic}],
  paths?, languages?, limit?, context_lines?, group_by?
) -> ranked matches + omitted counts + expansion cursor
```

Results MUST be bounded, deduplicated, revision-tagged, and addressable for focused reads. A “read all matching files” option is intentionally absent.

### 13.7 Index freshness

Filesystem events are hints, not truth. Kit increments workspace revisions for controlled edits, consumes watcher events for external edits, and periodically reconciles content hashes. Tree-sitter caches include grammar ABI and query hashes. LSP responses are tied to document versions. SCIP records are tied to commits and overlaid with live working-tree data.

## 14. Tool Surface

### 14.1 Design rules

Model-facing tools SHOULD be few, orthogonal, deterministic, and explicit about output bounds. Their descriptions are routing policy, not merely API documentation. A description states:

- what task shape should select the tool,
- when not to select it,
- what it saves or verifies,
- one compact validated example when the surface is unfamiliar.

Overlapping tools and large eager schemas increase input cost and can reduce selection accuracy and prompt-cache stability; magnitude is model- and task-dependent and MUST be measured.

### 14.2 Core tools

The default eagerly loaded set SHOULD be approximately:

```text
discover      ranked tree, symbols, and relationship expansion
search        batched lexical, structural, symbol, and semantic search
read          bounded files, ranges, symbols, artifacts, and diffs
edit          transactional multi-file patch with optional verification
run           sandboxed process with timeout/background policy
check         diagnostics, build, tests, lint, and affected checks
tools         deferred capability search, inspection, binding, and invocation
compose       bounded programmatic capability composition
agent         spawn/inspect/message/cancel ACP subagents or A2A tasks
yield         submit a semantic continuation and compact context
```

Provider-native server tools MAY be included when they outperform equivalent local tools and preserve policy.

### 14.3 Compact tool schemas

Kit preserves each capability’s source JSON Schema and declared dialect. Native Kit schemas SHOULD use JSON Schema 2020-12. Under MCP `2025-11-25`, a schema without `$schema` defaults to 2020-12 while an explicit alternate dialect is permitted. Provider adapters project schemas into each provider/model’s accepted subset and reject unsupported constraints rather than silently dropping them. Kit MUST preserve the source schema, normalized internal view, projection, and their digests.

The model-facing projection MAY use compact type notation:

```text
edit(patch: Patch, verify?: none|syntax|fast|targeted|full = fast)
  -> {revision, diff, checks[], diagnostics_delta, artifacts[]}

search(queries: {text: string, mode?: lexical|structural|symbol|semantic}[],
       paths?: string[], limit?: int = 50)
  -> {matches: Match[], omitted: int, cursor?: string}
```

Descriptions, defaults, enums, required fields, and important constraints remain present. Raw JSON Schema keywords that do not help the model are omitted from the projection. Author documentation MUST NOT be dropped.

The compact projection is generated one-way from the preserved schema and never parsed back into it. Kit benchmarks token count, call validity, field accuracy, reasoning-token use, and task accuracy per projection and model.

Eager provider-native tools still receive valid provider-supported JSON Schema. Compact notation replaces raw schema in deferred search/inspection and generic `tools.invoke`; it does not pretend a provider registration API accepts non-JSON schemas. Kit may generate a reduced provider-valid eager schema when doing so preserves every enforced model-relevant constraint.

## 15. Dynamic Tool Discovery

### 15.1 Capability catalog

Kit federates native tools, project plugins, MCP servers, ACP-exposed client capabilities, A2A peers, and provider-native tools into a catalog. Each entry stores:

- immutable capability identity,
- source and trust domain,
- source input/output schemas, dialects, normalized views, and projections,
- compact summary and search terms,
- schema and implementation digests,
- side-effect and idempotency classification,
- required capabilities and auth scopes,
- latency, cost, reliability, and usage statistics,
- version and availability.

### 15.2 Progressive discovery

The model-facing flow is:

```text
search summaries -> inspect exact definition -> bind schema digest -> invoke
```

Authorization filtering occurs before search so a principal cannot discover forbidden capabilities. A catalog change updates the index but MUST NOT silently replace a schema already bound to an active run.

`tools.bind` returns an immutable binding ID for `(source, capability, schema_digest, authorization_snapshot)`. If the provider supports dynamic deferred registration, Kit exposes that bound definition on the next model request. Otherwise the model calls `tools.invoke(binding_id, input)`, whose generic schema is eager and whose input is validated against the pinned bound schema before broker dispatch. Bindings expire on grant, catalog, or run-policy changes; schema changes require a new binding. This makes invocation and prompt-cache consequences explicit across providers.

Kit uses provider-native deferred tool search when available and validated, with a portable host implementation elsewhere. A small set of frequent safe tools stays eager. Large, tenant-specific, or rarely used catalogs remain deferred.

Negotiated MCP `notifications/tools/list_changed`, `notifications/resources/list_changed`, and `notifications/prompts/list_changed` events trigger coalesced refresh of the corresponding catalogs. They do not dictate what enters model context.

### 15.3 Tool learning

Kit records selection opportunities, searches, inspections, calls, errors, and outcomes. Offline analysis identifies:

- tools never selected because their descriptions are poor,
- tools selected for harmful task shapes,
- schema fields repeatedly misunderstood,
- high-value tool sequences suitable for composition or deterministic macros.

Description changes are evaluated as routing interventions, with both direct and competing surfaces available.

## 16. Programmatic Composition

### 16.1 Purpose

Composition replaces repeated model round-trips with one bounded program over the live capability catalog. It is most valuable for:

- pagination,
- list-then-detail N+1 access,
- deterministic filtering and aggregation,
- joins across tools,
- bounded fan-out,
- mechanical bulk reads or writes,
- retryable dataflow whose next steps are known in advance.

Composition SHOULD NOT be the default for exploratory investigation where each observation changes the next question, approval-sensitive effects, or a single cheap call.

### 16.2 Backends

Kit initially exposes both agentkit compose backends:

- Lua 5.4 for high model familiarity and low reasoning overhead;
- Runlet for schema checking, immutable dataflow, structured concurrency, bounded fan-out, and machine-legible diagnostics.

The backend is selected by measured `(model, task shape, risk)` policy. Lua is the economic default when models are more fluent in it. Runlet is preferred when static checking or concurrent dataflow materially improves correctness. Direct calls remain available.

In one local synthetic N+1 scenario, compose was cheaper for all ten tested models and improved mean rubric accuracy, but each model/scenario cell had one repetition. This is consistent exploratory evidence, not a universal result. The same study found composition more expensive on average for exploratory investigation. A separate local study found unfamiliar Runlet could consume substantially more hidden reasoning than shorter Lua while catching errors Lua missed. Kit treats model and task shape as interactions to reproduce with repeated, paired trials.

### 16.3 Execution semantics

Every nested invocation passes through the same capability broker as a direct call. Composition does not bypass:

- schema validation,
- authorization and approvals,
- secret handling,
- tracing and cost accounting,
- call count, concurrency, CPU, memory, output, and wall-time budgets,
- cancellation,
- side-effect classification,
- idempotency and retry policy.

Approval of a program is not blanket approval of all runtime effects. Kit can grant bounded categories such as “read these capabilities at most 100 times.” Calls outside the grant pause or fail.

Side-effecting calls require idempotency keys when supported. A composition is not transactional across remote systems; partial effects are recorded explicitly.

### 16.4 Output minimization

Only the final selected value and necessary diagnostics enter model context. The full execution graph and nested results remain available as artifacts and events. Programs SHOULD aggregate, assert, and return compact evidence rather than source records.

### 16.5 Diagnostics as interface

Compiler and runtime errors MUST be concise, located, specific, and repairable. Known model confusions deserve targeted diagnostics and fix suggestions. Validated exemplar programs are compiled in CI so documentation cannot drift from the language.

## 17. TOON and Result Encoding

TOON is a presentation encoding for JSON-shaped values, not a schema language or canonical wire format. Kit uses each protocol’s required representation: JSON-RPC/JSON for ACP and MCP; a selected A2A 1.0 binding such as JSON-RPC, HTTP+JSON, or gRPC/Protobuf; and separately versioned Kit API and storage representations.

For model context, Kit chooses among compact JSON, TOON, plain text, tables, and artifact handles using structural eligibility and actual tokenizer measurements. TOON is favored for uniform arrays of objects and rejected when nested or irregular data makes it larger or less clear.

The tool result records canonical structured content plus a model presentation outside the transcript. Only the selected presentation enters model context; canonical JSON remains addressable and powers validation and composition. Kit initially pins TOON Working Draft 3.3 and its conformance tests. Because `text/toon` is provisional and unregistered, presentation metadata is internal rather than advertised as a standardized Internet media type:

```text
semantic content: application/json
presentation: {encoding: toon, spec_version: "3.3"}
```

Strict decoding and length/width checks are required. TOON content remains untrusted data and receives the same prompt-injection and downstream-escaping treatment as JSON or text.

In a local Sonnet-5 compose benchmark over five synthetic scenarios and two backends, three repetitions per cell showed about 9% lower suite cost with TOON and unchanged rubric accuracy; all 30 TOON runs scored 1.00. It was not a high-powered interleaved A/B, per-cell cost was dominated by noisy hidden reasoning, and lower-tier models were not tested. Kit treats this as a payload-specific replication hypothesis, not evidence for global enablement.

## 18. Transactional Editing

### 18.1 Canonical edit IR

All model-specific edit formats normalize to:

```text
AddFile
DeleteFile
MoveFile
ReplaceRange
```

Each request includes a workspace revision, base content hashes, and exact textual or semantic anchors. Durable edit addresses are not AST node identities because tree shape changes across edits and parser versions.

### 18.2 Model-facing formats

Kit supports:

- simplified context patch as the general default,
- exact search/replace for local edits,
- whole-file output for new or very small files,
- LSP `WorkspaceEdit` for semantic rename and code actions,
- `ast-grep` recipes for repeated structural transformations,
- language-native codemods where available.

The model/operation router chooses the format from measured patch-apply and task success. Standard unified diffs are accepted for interchange but models are not required to generate line counts.

### 18.3 Edit transaction

An `edit` call performs:

1. acquire the managed workspace mutation lock;
2. resolve and authorize paths from pre-opened workspace-root handles with no-follow component checks;
3. verify workspace revision and base hashes;
4. parse the entire request before mutation;
5. reject absent or ambiguous anchors;
6. stage all declared files in a complete copy-on-write execution view;
7. apply requested operations exactly;
8. parse changed files and detect new syntax errors;
9. run configured formatters against staged content;
10. reparse formatter output;
11. collect bounded LSP diagnostic deltas through a shadow document/workspace when supported;
12. run the requested verification profile against the staged view;
13. write and sync a recovery manifest, complete undo images including file types/metadata, and staged artifacts before replacing any file;
14. materialize and sync the complete declared change, excluding unrelated check outputs;
15. append the workspace revision event and mark the manifest committed;
16. update indexes/LSP buffers and return the actual diff, checks, diagnostics, and artifacts.

Kit MUST reject low-confidence fuzzy application rather than silently editing the wrong duplicate. An enclosing symbol can disambiguate an otherwise exact repeated anchor.

For staged diagnostics, Kit sends versioned staged buffers to an isolated LSP session or shadow workspace using `didOpen`/`didChange`, collects responses tied to those document versions, then closes or discards the shadow state. It MUST NOT expose staged buffers to the live workspace LSP. If a language server cannot diagnose shadow content safely, pre-commit LSP diagnostics are unavailable for that adapter; compiler checks run in the copy-on-write execution view and live diagnostics are collected only after commit.

Multi-file host filesystem writes and database events cannot share one physical transaction. The idempotent manifest states are `staged -> prepared -> materialized -> committed` or `rolled_back`. Any failure before the revision event restores and syncs the complete undo image; after the event, recovery rolls materialization forward. Recovery runs under the mutation lock before an in-process error returns and again at startup before the workspace is exposed. Kit calls the operation transactional only when staging, locking, journaling, and recovery prevent an agent from observing or continuing from a partial logical revision.

### 18.4 Auto-verification flag

The edit tool’s extra `verify` flag removes a model round-trip when the next action is predictable:

```text
none      apply only
syntax    parse and format
fast      syntax, format, bounded diagnostics
targeted  fast plus affected typecheck/tests
full      configured full verification policy
```

`fast` is the default. Path, revision, anchor, syntax, formatter, sandbox, and materialization failures are hard gates and never commit. `targeted` and `full` test/typecheck failures use `on_check_failure: commit|abort`; the default is `commit` so the verified failing edit becomes the next repair revision. With `abort`, checks still run against the staged view but no source edit is materialized. Cancellation before prepare aborts; cancellation after prepare completes recovery before another writer proceeds. The result contains only diagnostic deltas and concise failures, with complete logs in artifacts.

Example:

```json
{
  "status": "applied_with_failed_checks",
  "revision": 185,
  "diff": "artifact:blake3:...",
  "verification": {
    "profile": "targeted",
    "status": "failed",
    "checks": [
      {"name": "rustfmt", "status": "passed", "ms": 41},
      {"name": "auth::refresh_rotation", "status": "failed", "ms": 813,
       "summary": "expected old token rejection, got success"}
    ],
    "new_diagnostics": [],
    "resolved_diagnostics": ["src/auth/token.rs:42 E0308"]
  }
}
```

### 18.5 Concurrent changes

Transactional guarantees apply to daemon-exclusive managed workspaces. External modifications advance or invalidate the workspace revision. Attached user checkouts are explicitly cooperative: Kit takes an advisory external-writer lock where supported and rechecks every base hash immediately before materialization, but cannot promise atomicity against an uncooperative editor. A stale edit returns a compact conflict containing changed paths and relevant hunks. Kit never overwrites concurrent user work through a whole-file rewrite without matching its base hash.

Ordinary `run`, build, formatter, test, LSP, and MCP processes see source read-only and write only to designated build/temp paths. An explicitly source-writable process must acquire the same mutation lock, receives a dedicated overlay, and on exit either promotes a declared diff as one revision or discards it. Kit confirms the old sandbox is quiescent before reassigning a workspace; a lease token alone cannot fence an escaped OS process.

## 19. Verification

### 19.1 Verification ladder

Kit uses the cheapest high-signal checks first:

```text
patch validity
-> syntax and formatter
-> changed-file diagnostics
-> typecheck or build slice
-> reproducing/targeted tests
-> nearby regression tests
-> affected package/service checks
-> full suite, security, performance, or integration checks
```

The defect SHOULD be reproduced before editing when feasible. Existing red builds and diagnostics are baselined; decisions use deltas rather than assuming the repository starts clean.

### 19.2 Affected-check selection

Test selection combines:

- explicit user commands and repository policy,
- changed paths and package ownership,
- symbol-to-test graph edges,
- build-system dependency graphs,
- historical co-change and failure data,
- coverage when available,
- model proposals under deterministic validation.

Critical checks are never omitted solely because a learned selector predicts low risk.

### 19.3 Feedback shape

Failures returned to the model contain:

- command/check identity and exit status,
- failing test or diagnostic identifiers,
- the first relevant stack frame and source location,
- expected versus actual values,
- change from the previous run,
- artifact handle for complete logs.

Repeated full logs MUST NOT accumulate in context.

### 19.4 Loop control

Each run has budgets for model turns, reasoning, spend, wall time, test time, patch size, repeated failures, and destructive effects. After two materially identical failures, policy SHOULD trigger re-localization, model escalation, a fresh-context reviewer, or user input rather than another equivalent edit.

Independent review can find omissions, but findings are not authoritative until executable checks or maintainers validate them.

## 20. Model and Strategy Routing

### 20.1 Router inputs

Routing uses task class, repository/language, estimated difficulty, ambiguity, risk, context size, tool availability, provider health, cache locality, latency/cost SLO, and observed failure state.

### 20.2 Phase routing

Classical code or fast inexpensive models SHOULD handle:

- classification and query expansion,
- tool search ranking,
- log and result compression,
- simple formatting and schema projection,
- straightforward low-risk edits,
- verification-result classification.

More capable models SHOULD handle:

- ambiguous requirements,
- multi-file localization,
- architectural or public API changes,
- concurrency, security, migrations, and state bugs,
- lower-tier failures,
- high-cost irreversible effects.

Reasoning effort is routed independently from model size and visible verbosity.

### 20.3 Escalation

Escalation is triggered by low localization confidence, disagreement, cross-boundary scope, failed verification, sensitive code, repeated patch conflicts, or an expected downside above policy thresholds. Test-failure evidence generally outranks a prompt-only difficulty prediction.

### 20.4 Online learning safety

Production routing changes use logged counterfactual features, offline replay only where causally valid, shadowing, and canaries. Kit never lets an unbounded online learner directly authorize tools or destructive actions.

## 21. Parallelism and Speculation

Kit parallelizes independent lexical/symbol/semantic searches, hypothesis investigation, test shards, static checks, and isolated candidate patches. Dependencies remain explicit.

Multiple writing agents MUST use separate workspaces or copy-on-write snapshots. Results merge through reviewed patches, never concurrent writes to one checkout.

Speculation is allowed only when expected critical-path savings exceed wasted cost and resource contention. Examples include warming an environment, starting likely test shards, or producing diverse candidate patches in isolated workspaces.

Kit records canceled and unused speculative spend. Diversity SHOULD come from different evidence, retrievers, hypotheses, or models rather than temperature alone.

Backpressure and weighted fair scheduling protect interactive p95 latency from bulk indexing, evaluations, and large subagent trees.

## 22. Smart Model-Driven Compaction

### 22.1 Semantic checkpoint

The model can call the `yield` control tool when it reaches a natural stopping point: a hypothesis is resolved, an implementation phase is complete, verification changes the plan, or context contains substantial superseded material.

The call supplies a compact continuation packet:

```text
goal and acceptance criteria
current workspace revision
files changed and why
decisions and constraints
verified facts with source handles
failed hypotheses and why
checks run and exact unresolved failures
open questions
next intended action
must-retain transcript/artifact references
```

This is a control action, not user-facing prose. `yield` MUST be the sole tool call in its model round and is accepted only when no tool, approval, auth flow, edit transaction, child merge, or other blocking operation is in flight. A mixed or mid-flight call is rejected with a compact, retryable control error and does not alter context. A valid call ends the current model round; after validation, Kit compacts and returns a small acknowledgment before the next model call. The continuation packet has a model- and task-specific maximum token budget.

### 22.2 Checkpoint validation

Kit enriches and validates the proposal against authoritative state:

- changed files and revision come from the workspace;
- checks and outcomes come from verification events;
- user requirements are copied from the task contract;
- referenced artifacts must exist;
- tool call/result protocol pairs remain valid;
- omissions detected by deterministic rules are appended.

The model cannot erase an unmet requirement by omitting it from a summary. Kit accepts a checkpoint automatically when schema validation passes, all authoritative fields reconcile, all required facts fit the packet budget, and no blocking operation is in flight. Otherwise it returns a compact validation error and retains the existing context; repeated rejection falls back to deterministic compaction rather than a summary repair loop.

### 22.3 Context replacement

On acceptance, Kit persists the checkpoint and builds the next model history from:

```text
stable prefix + structured continuation packet + recent unresolved evidence
```

Older transcript content remains losslessly addressable by event or artifact handle. The next model call can retrieve it, but does not pay for it by default.

### 22.4 Automatic fallback

Agentkit’s `LoopMutator` and compaction pipeline provide structural and semantic fallback when the model does not call `yield`. Kit first clears low-risk material: stale raw logs, superseded maps, duplicate reads, reasoning parts, and successful command noise. It summarizes only after deterministic eviction is insufficient.

Automatic compaction triggers at a configured fraction of the model context window or a lower learned threshold, before severe saturation, and only at valid mutation points such as after a tool result or turn end. The exact threshold is recorded with each run and has a deterministic default. Compaction quality is tested as state transfer: a resumed agent must retain every requirement, changed file, active failure, rejected approach, and next action.

## 23. Agents and Protocol Boundaries

### 23.1 Protocol roles

Kit uses three complementary protocols:

| Protocol | Kit role |
| --- | --- |
| ACP | Coding client to Kit, and Kit to coding subagent processes |
| A2A | Collaboration and durable task delegation between autonomous peer agents |
| MCP | Discovery and invocation of tools, resources, and prompts |

They MUST NOT be flattened into one generic protocol merely because they share JSON-RPC-like concepts.

### 23.2 ACP clients

Kit exposes stable ACP protocol version 1 so editors and lightweight clients can create sessions, send prompts, receive streamed messages/tool updates/diffs, resolve permissions, and use supported filesystem or terminal callbacks. Durable reopening uses optional v1 `session/load` only when negotiated and implemented. The reviewed `agentkit-acp` headless runtime does not yet implement durable load semantics, so Kit must add them. ACP session IDs map to Kit threads and runs, not directly to process-local `LoopDriver` state.

ACP v2 or remote transports remain negotiated features until stable. Kit uses the official SDK through `agentkit-acp` rather than maintaining parallel wire types.

### 23.3 ACP subagents

An ACP subagent is an independently supervised child process or endpoint with:

- a child Kit run and parent link,
- a scoped workspace snapshot,
- explicit prompt and acceptance criteria,
- bounded model, tool, process, and time budgets,
- capability grants narrower than or equal to its parent,
- streamed updates and durable final artifacts.

Kit acts as ACP client to the subagent. This makes third-party coding agents replaceable and observable. An in-process agentkit child MAY be optimized internally, but its durable behavior is normalized to the same child-run contract.

Parents do not receive entire child transcripts by default. They receive compact progress, evidence, and result artifacts. Independent children can run concurrently. Child writes merge only through explicit patches or commits.

For a local child, Kit owns spawn, ACP initialization, session, heartbeat, cancellation, process exit, and cleanup. For a remote ACP endpoint, it owns the authenticated connection and task relationship only. Disconnect triggers bounded reconnect/load where supported; otherwise the child attempt becomes `interrupted`. Cancellation is terminal only after acknowledgment or confirmed local process quiescence; unresolved remote state is `outcome_unknown`.

### 23.4 A2A peers

Kit initially implements A2A protocol `1.0.0` and exposes selected autonomous skills through an Agent Card. It can delegate to remote A2A agents. A2A tasks map to durable Kit runs or external task links with separate task, context, artifact, authentication, idempotency, and trace IDs.

A2A is used when the remote party owns its own planning and state. MCP is used when Kit invokes a capability. Raw internal tools are not automatically advertised as A2A skills.

`ExternalTask` maps A2A `submitted`, `working`, `input-required`, `auth-required`, `completed`, `failed`, `canceled`, and `rejected` states without treating them as local attempt states. `auth-required` creates a durable Kit auth request; `rejected` is terminal. A message-only response is retained as a completed exchange without inventing a remote task. Messages and artifacts are deduplicated by remote identity and sequence/digest. Delegation carries a depth/path token to reject loops. Timeouts and transport loss yield `interrupted`, then bounded retry or user policy; non-idempotent submissions are never blindly duplicated. Cancellation is an idempotent remote request and becomes terminal locally only after peer confirmation; otherwise outcome remains unknown.

Push notifications and remote artifact URLs are authenticated, replay-protected, and SSRF-filtered. If a card carries signatures, or policy requires them, Kit verifies them against configured trust roots and binds card identity to authenticated transport and authorization policy. Unsigned cards are accepted only where policy explicitly permits them.

### 23.5 MCP broker

Kit connects stdio and Streamable HTTP MCP servers through `agentkit-mcp`, preserving tools, resources, and prompts as distinct capability types. It supports explicit discovery, list-change refresh, auth interruption/resume, and configured client-side sampling, elicitation, and roots responders.

MCP servers and their descriptions are untrusted unless explicitly trusted. Tools execute through Kit’s normal broker with pinned schema digests. Credentials never enter model context or composition programs.

Kit’s capability broker is the sole policy and dispatch authority for native tools, MCP tools/resources/prompts, ACP callbacks, and MCP sampling/elicitation/roots. Agentkit permission types adapt broker decisions into loop interrupts; they are not a second authority. Protocol permission or elicitation responses resolve a durable Kit request only after actor authentication and grant validation, with their own causation, budget, and cancellation records.

## 24. Capability Security

Permissions are capabilities scoped by:

```text
principal
project and workspace
tool/capability identity and schema digest
argument constraints
effect class
call and byte limits
time window
network destination
credential handle
parent run and delegation depth
```

Repository text, tool descriptions, MCP metadata, web content, and agent messages are data, not authority. Prompt instructions cannot expand grants.

Secrets are opaque handles resolved just in time by the broker. Secret values MUST NOT appear in prompts, events, traces, composition source, retained terminal history, or workspace metadata. They SHOULD NOT appear in process arguments because argv is commonly observable; file descriptors, memory-backed files, environment injection where unavoidable, short-lived credentials, and egress-proxy injection are preferred. If a target program only accepts argv credentials, policy requires explicit approval, redaction at capture boundaries, and the shortest feasible lifetime.

Every discovered URL is defended against SSRF, redirects, DNS rebinding, private ranges, local services, dangerous schemes, and unauthorized hosts.

## 25. Process and Workspace Isolation

### 25.1 Trust tiers

| Tier | Intended use | Boundary |
| --- | --- | --- |
| trusted local | one developer and trusted repository | OS sandbox by default; explicit weaker host mode uses scrubbed environment, process group, and limits |
| restricted | untrusted repository in one security domain | rootless container or OS sandbox, namespaces, syscall/filesystem policy, no network by default |
| hostile multi-tenant | mutually untrusted users | gVisor or per-run microVM, isolated storage and credentials |

The daemon host is never the default execution environment for repository code, build scripts, language servers, formatters, package managers, or MCP subprocesses. Unsandboxed host execution is an explicit opt-in compatibility mode and is not described as isolation or fail-closed.

### 25.2 Executor requirements

Executors enforce:

- source read-only by default, with writable build/temporary paths and explicit mutation overlays only;
- no Docker socket, SSH agent, host daemon socket, cloud metadata, or unrelated home directories;
- scrubbed environment and explicit secret injection;
- network deny by default with destination grants;
- CPU, memory, PID, file size, disk, I/O, and wall-time limits;
- whole-tree cancellation and reaping;
- symlink, hard-link, path traversal, and mount escape defenses;
- fail-closed behavior when required isolation is unavailable.

On Linux, dedicated cgroup v2 trees and `cgroup.kill` SHOULD own process cleanup. macOS local execution uses Seatbelt where practical and a VM for hostile work. Windows uses Job Objects and an appropriate container/VM boundary. Shell authority is expressed as the complete executor profile: mounts, source-write mode, credentials, egress, limits, and sandbox tier. Command-name parsing is only an additional restriction.

### 25.3 Workspaces

Each writing run receives a Git worktree, clone, or copy-on-write snapshot. Parallel subagents receive separate snapshots. Restricted or hostile execution uses per-run clones/COW repositories or mediated Git operations; it never mounts a shared writable Git common directory into the sandbox.

Kit records base commit, initial dirty-state hash, workspace revision, final diff, and resulting revision. It preserves unrelated user changes and never invokes destructive Git operations without explicit authority.

Hooks and submodules are treated as executable code and disabled or sandboxed by default.

### 25.4 Terminals

Pipes are the default. PTYs are allocated only for terminal semantics. A terminal has one input-writer lease, multiple read-only viewers, sequenced output chunks, resize events, retention policy, and an owning process.

Raw terminal input is not persisted by default because it frequently contains secrets. If the daemon owning a local PTY dies, the attempt is interrupted; Kit does not claim resumability it cannot provide.

## 26. Cancellation, Scheduling, and Recovery

Cancellation is a durable request:

1. append cancellation intent;
2. cancel the agentkit/controller token;
3. stop issuing new model and tool work;
4. interrupt interactive processes;
5. terminate after a grace period;
6. kill the complete sandbox/process tree;
7. reap resources and record outcomes;
8. enter `cancelled` only when all locally owned effects are confirmed quiescent and remote cancellation is acknowledged.

Each active attempt holds a renewable lease and fencing token. Effects and completion events carry that token. A workspace or run is not reassigned until the executor confirms the old sandbox is quiescent; unconfirmable local or remote effects end as `interrupted` or `failed` with `outcome_unknown`, never `cancelled`. An outcome-unknown interruption blocks automatic resume until reconciliation or an explicit policy decision. On ambiguous crash recovery, Kit inspects the executor where possible and does not blindly repeat non-idempotent work.

Scheduling uses bounded queues, per-principal and global concurrency, subagent-depth limits, reserved spend, and weighted fairness. Interactive runs take latency priority over offline indexing and benchmarks without starving background work.

## 27. Public API

### 27.1 Transport

Kit exposes a versioned HTTP JSON API described by OpenAPI. Every transport is authenticated and every resource, event, terminal, and artifact is authorized to a principal/project. Local Unix sockets use state-root permissions and peer credentials plus a session token where needed. Loopback HTTP uses a random bearer credential and strict Origin/Host checks. Remote HTTP and ACP entry points require caller authentication through mTLS identity or validated OAuth/OIDC bearer tokens with issuer, audience, expiry, revocation, and principal mapping; server-authenticated TLS alone is insufficient. Server-sent events provide resumable event streams. WebSocket is reserved for bidirectional terminal attachment.

Representative resources:

```text
POST   /v1/projects
GET    /v1/projects/{id}/retention
PATCH  /v1/projects/{id}/retention
POST   /v1/threads
GET    /v1/threads
GET    /v1/threads/{id}
POST   /v1/threads/{id}/archive
DELETE /v1/threads/{id}
POST   /v1/threads/{id}/runs
GET    /v1/threads/{id}/events?cursor={cursor}
GET    /v1/runs
GET    /v1/runs/{id}
POST   /v1/runs/{id}/cancel
POST   /v1/runs/{id}/input
GET    /v1/runs/{id}/cost
GET    /v1/runs/{id}/timeline
GET    /v1/runs/{id}/agents
GET    /v1/runs/{id}/checkpoints
GET    /v1/runs/{id}/prompts
GET    /v1/workspaces/{id}/diff
GET    /v1/approvals?status=pending
POST   /v1/approvals/{id}/resolve
GET    /v1/auth-requests?status=pending
POST   /v1/auth-requests/{id}/resolve
GET    /v1/processes
GET    /v1/processes/{id}
GET    /v1/terminals/{id}
GET    /v1/terminals/{id}/attach
GET    /v1/artifacts/{id}
GET    /v1/capabilities
GET    /v1/experiments/{id}
```

`POST /threads/{id}/runs` atomically records one input message and creates or returns its run. HTTP, CLI, and ACP prompts route through this one command, preventing duplicate message/run semantics.

Non-naturally-idempotent mutations, especially run/message creation, MUST include `Idempotency-Key`; first-party clients generate and retain it through the retry window. The key is scoped to authenticated principal, command, and target. The command transaction stores a canonical request digest and response; reuse with different input returns conflict, while pending and terminal outcomes replay. Keys have a declared retention period no shorter than the maximum retry window. Long-running work returns `202 Accepted` with an existing domain resource. Errors use RFC 9457 problem details. IDs are opaque, timestamps are RFC 3339 UTC, and pagination/events use opaque cursors.

Clients MUST ignore unknown additive fields and event types. Event payloads carry independent schema versions.

Archive is a reversible visibility state. Delete creates an auditable asynchronous deletion job governed by project retention, legal hold, shared-artifact reachability, and backup expiry; it never silently removes still-referenced content. Retention APIs expose effective event, transcript, terminal, artifact, experiment, and backup policies plus the earliest possible physical-deletion time.

### 27.2 CLI parity

The CLI calls the same command/query service as external clients. It has no hidden agent capability. When embedded/local direct calls avoid HTTP serialization, they still pass through identical authorization, validation, event, and lifecycle handlers.

The local daemon owns one configured state root and acquires its exclusive lock before migrations or serving. Endpoint and credential discovery use a permission-restricted file in that root. `kit daemon` runs foreground unless explicitly daemonized; CLI auto-start is opt-in and waits for readiness. Graceful shutdown stops admission, checkpoints safe runs, cancels or hands off owned work according to policy, reconciles prepared edits, flushes events, and releases the lock.

Representative commands:

```text
kit daemon
kit thread create
kit run start --thread ... --message ...
kit run show ...
kit run cancel ...
kit events --follow --cursor ...
kit approval resolve ...
kit process list
kit terminal attach ...
kit workspace diff ...
kit tools search ...
kit eval run ...
kit status
```

All query commands support human output and `--json`; streams support `--jsonl`.

### 27.3 Third-party clients

OpenAPI, stable event envelopes, ACP support, and ordinary HTTP/SSE make third-party clients trivial. No client must embed Rust, agentkit, or Kit’s storage schema.

## 28. Observability

Kit emits OpenTelemetry-compatible traces, metrics, and structured logs behind a versioned internal telemetry adapter.

Trace hierarchy:

```text
api.command
  run.attempt
    model.call
    tool.call
      nested.tool.call
      process.exec
    child.run
    verification.check
    compaction.checkpoint
```

The built-in read-only web application provides simple observability, not a full IDE. It derives every view from public run/thread projections, timeline, agent-tree, checkpoint, approval, process, cost, artifact, and event endpoints:

- threads and run state,
- live event timeline,
- model/tool/process spans,
- token, cache, cost, and latency breakdowns,
- workspace diff and verification evidence,
- subagent/A2A tree,
- pending approvals and auth,
- active processes and terminals,
- prompt/checkpoint versions with sensitive content redacted,
- experiment assignment and outcome.

Metrics avoid high-cardinality labels such as run IDs, paths, prompts, or commands. Those belong in access-controlled traces and logs.

Health endpoints distinguish liveness from readiness. Slow event clients are disconnected after bounded buffering and resume from durable cursors.

## 29. Cost and Latency Optimizations

Kit pursues the following adjacent optimizations as independently measurable policies.

### 29.1 Reduce sequential model work

- constrain human-facing verbosity;
- emit minimal patches instead of unchanged file content;
- use structured control calls instead of prose;
- use grammar-constrained structured output where provider support improves syntax without semantic regressions;
- batch independent tools;
- compose dependent mechanical calls;
- execute loops, joins, and reductions outside the model;
- memoize only deterministic, revision- and authorization-keyed reads;
- use provider predicted-output features for mostly unchanged whole-file generation only when measured savings exceed rejected-token cost;
- stop retries when expected marginal value falls below cost.

### 29.2 Reduce prefill and context

- stable exact prefixes and canonical serialization;
- deferred tool schemas;
- targeted code slices and personalized maps;
- artifact handles instead of repeated logs;
- model-selected checkpoints and deterministic eviction;
- compact schema notation and adaptive result encoding;
- session affinity when it improves cache hits without harmful load imbalance.

### 29.3 Reduce environment latency

- warm repository mirrors and copy-on-write images;
- persistent language servers keyed by safe workspace boundaries;
- dependency, compiler, build, and test-discovery caches;
- prebuilt sandbox images;
- persistent provider and MCP connections;
- incremental parsing and indexes;
- offline batch APIs for non-interactive summarization, indexing, and evaluation.

### 29.4 Improve decision efficiency

- phase- and risk-aware model routing;
- confidence-calibrated abstention and escalation;
- expected-value-of-information ranking for reads and tests;
- learned affected-test selection with deterministic safety floors;
- diverse hypothesis fan-out only for high uncertainty;
- fresh-context review only where its defect yield justifies cost.

### 29.5 Self-hosted inference

When Kit operates self-hosted models, it SHOULD evaluate:

- continuous batching,
- paged KV-cache allocation,
- automatic prefix caching,
- grouped-query and quantized KV where model-compatible,
- speculative decoding,
- prompt-prefix sharing across workers,
- prefill/decode disaggregation,
- admission control by predicted sequence length.

Every quantization or decoding optimization must pass the same coding and tool-call evaluations. Throughput gains do not excuse changed outputs or higher tail latency.

## 30. Evaluation Program

### 30.1 System under test

The evaluated unit is:

```text
model + prompt + context + tools + router + sandbox + retry policy
+ verifier + cache state + infrastructure
```

Model-only comparisons are insufficient.

### 30.2 Evaluation portfolio

Kit maintains:

- public benchmarks for comparability;
- private recent repository tasks for product validity;
- adversarial and permanent regression suites;
- production shadow and canary experiments.

Public coverage SHOULD include pinned dataset and harness releases of SWE-bench Verified and Multilingual; separately maintained SWE-bench-Live for recent tasks; SWE-bench Multimodal for image-capable configurations; Terminal-Bench 2.1 for terminal competence; and task-specific benchmarks where appropriate. Reports identify exact instance sets, harness commits, exclusions, and task distributions. Public benchmark limitations, contamination, language skew, patch size, and harness quality are reported alongside scores.

Private tasks include bugs, features, migrations, dependency updates, refactors, tests, CI repair, incident diagnosis, review feedback, documentation, security-sensitive changes, and tasks that should ask or abstain. They are stratified by language, repository size, task class, human duration, files touched, ambiguity, test quality, and risk.

### 30.3 Harness requirements

Every trial pins:

- repository commit and task version,
- container/VM image digest and architecture,
- CPU, memory, disk, network, process, time, turn, token, and dollar budgets,
- model/provider snapshot and reasoning settings,
- prompt, tool, router, verifier, and scaffold digests,
- cache condition,
- hidden grader version,
- randomization identifiers and provider request IDs.

The harness uses a fresh isolated environment per trial. Gold patches and hidden tests remain outside the agent sandbox. It stores the final tree, patch, events, model/tool usage, checks, and grader result.

Harness validation checks that the original state fails intended assertions, the reference solution passes, and empty, malformed, and adversarial patches fail. Repeated regrades under the pinned environment estimate residual nondeterminism; observed disagreement and flake rates are reported rather than retried away.

### 30.4 Statistical method

Comparisons are randomized in time blocks, paired at the task/block level where feasible, repeated for stochastic systems, and stratified by repository and task class. Reports include effect sizes and confidence intervals. Sample size, estimand, correctness non-inferiority margin, stopping rule, and multiplicity family are predeclared.

With one binary pair per task, exact McNemar is appropriate. With repeated trials or tasks clustered by repository, Kit analyzes task-level paired summaries or uses a hierarchical model, GEE, or repository-then-task cluster bootstrap/randomization that retains all trials. Cost and latency use paired bootstrap or randomization tests; tail quantiles receive bootstrap intervals. Multiple confirmatory comparisons are corrected. Non-inferiority is claimed only when the appropriate one-sided confidence bound excludes the margin.

HumanEval `pass@k` is used only for a fixed candidate-sampling policy, with estimator and sample count reported; it assumes an oracle can recognize a passing candidate. Adaptive retries, repair turns, and verifier-selected candidates are evaluated end to end as `resolution@budget`, including selection errors and all costs. `pass^k` is used only for the predeclared probability that all `k` fresh runs of one fixed policy succeed and is estimated empirically rather than replaced with `(pass@1)^k` without an independence argument.

### 30.5 Required ablations

Kit runs ablations for:

- each standing prompt module;
- lexical, syntax, semantic, graph, history, and embedding retrieval;
- static versus personalized repo maps;
- full JSON Schema versus compact projections;
- JSON versus TOON by payload shape;
- direct, parallel, Lua, and Runlet tool use;
- each edit format and verification flag;
- fixed versus routed model/reasoning effort;
- cold versus warm prompt and infrastructure caches;
- structural versus semantic/model-selected compaction;
- serial, batched, parallel, and speculative execution;
- retry, review, and escalation policies.

Factorial designs are used when interactions are likely, such as caching with compaction or model tier with composition backend.

### 30.6 Retrieval evaluation

Discovery is evaluated independently from generation using file/symbol recall@k, reciprocal rank, provenance correctness, evidence tokens, time to first relevant symbol, index latency/freshness, and downstream task resolution. Oracle localization estimates the maximum value retrieval improvements can provide.

### 30.7 Editing evaluation

Edit benchmarks measure apply success, correct-location rate, parse/typecheck/test success, unintended changed lines, diff size, output tokens, latency, conflict detection, and recovery turns. Cases include Unicode, CRLF, missing newlines, duplicate anchors, renames, symlinks, concurrent modifications, and binary files.

### 30.8 Production experiments

Shadow runs do not apply output. They measure routing, costs, permissions, and delayed ground truth under real workload. Canary rollout uses sticky assignment and watches acceptance, developer active time, review/rework, rollback, defects, unauthorized actions, p95/p99 latency, and cost per accepted task.

Security failures trigger automatic rollback. Canaries run long enough to observe review and post-merge outcomes.

### 30.9 Replay limits

Kit distinguishes:

- regrading a fixed artifact,
- replaying recorded tool results through parsers or presentation,
- replaying fixed model responses through scaffold code,
- forking an environment checkpoint and rerunning,
- a fresh rollout.

Once a policy chooses a different action, recorded future observations are not a valid causal estimate. Fresh rollouts are required for agent quality claims.

### 30.10 Reward-hacking defenses

Graders are immutable and external. Hidden mutation/metamorphic tests, adversarial patches, side-effect checks, least privilege, paired act/do-not-act tasks, and manual trace audits defend against test deletion, grader probing, premature completion, and proxy optimization.

Kit MUST NOT optimize tool count, changed lines, tests run, speed, or token count as if any were correctness.

## 31. Configuration and Extensibility

Configuration has explicit layers:

```text
built-in safe defaults
user configuration
project configuration
thread/run overrides
experiment assignment
```

Later layers cannot expand security authority beyond the authenticated principal’s grants. Every effective configuration is materialized with a digest on the run.

Extensions can provide:

- model adapters through agentkit,
- native capability providers,
- MCP servers,
- ACP subagents and clients,
- A2A peers and skills,
- repository language/index adapters,
- edit and verification adapters,
- prompt modules and policies,
- executor backends,
- cost tables and experiment policies.

In-process plugins are trusted code. Untrusted extensions run out of process through MCP, ACP, A2A, or a narrow Kit plugin protocol under sandbox policy.

## 32. Compatibility and Versioning

Kit independently pins and negotiates:

- public API major version,
- event payload schema version,
- persistence schema version,
- agentkit crate version,
- ACP wire version,
- A2A interface/protocol version,
- MCP revision date,
- JSON Schema dialect,
- TOON version,
- Tree-sitter grammar/query versions,
- LSP position encoding and server version,
- SCIP schema/index version,
- provider model snapshot and feature version.

Similar JSON-RPC envelopes do not imply equivalent lifecycle, cancellation, streaming, error, or auth behavior. Every adapter has conformance and round-trip tests.

## 33. Risks and Mitigations

| Risk | Mitigation |
| --- | --- |
| Terse prompts under-specify work | preserve explicit goal, constraints, acceptance criteria; deletion ablations |
| Context pruning loses a decisive fact | lossless artifacts, structured checkpoint validation, state-transfer evals |
| Retrieval misses distant dependencies | hybrid sources, bounded graph expansion, escalation, oracle analysis |
| Semantic indexes are stale | revision tags, live LSP/Tree-sitter overlay, reconciliation |
| Compact schema drops meaning | one-way projection, preserve descriptions/constraints, schema digest and evals |
| TOON harms irregular payloads | tokenizer- and shape-adaptive selection, canonical JSON retained |
| Compose concentrates program risk | task-shape routing, static Runlet option, assertions, bounded direct fallback |
| Unfamiliar DSL increases reasoning cost | Lua default where cheaper, model/backend routing, measure hidden output |
| Nested tools bypass permissions | one broker for direct and nested calls, bounded grants, full traces |
| Transaction partially materializes | staged overlay, journal, revision fencing, recovery |
| Formatter changes unrelated code | separate formatter diff, changed-range mode where supported |
| Existing red repository causes loops | baseline and compare diagnostic/check deltas |
| Parallel agents conflict | isolated snapshots and explicit merge artifacts |
| Speculation lowers latency but explodes cost | learned value threshold, quotas, record canceled spend |
| Model router misses hard-looking-simple tasks | verification-triggered escalation and risk floors |
| Daemon crash duplicates effects | durable intent/outcome, idempotency, leases, fencing, `outcome_unknown` |
| Child process escapes cancellation | cgroup/job/sandbox ownership and whole-tree kill |
| Prompt injection gains authority | separate data from capability grants; broker enforcement |
| Secret leakage | opaque handles, narrow injection, redaction, retention controls |
| Protocol churn | adapters, independent version pins, conformance suites |
| Benchmark overfitting/contamination | private rolling tasks, frozen holdouts, preregistration, canaries |
| Observability leaks code or prompts | access control, redaction, encryption, configurable retention |
| Event volume overwhelms storage | semantic events plus chunked artifacts and retention classes |

## 34. Delivery Architecture

Implementation may proceed in dependency-ordered slices while preserving this RFC’s boundaries:

| Slice | Deliverable | Exit criteria |
| --- | --- | --- |
| `KIT-MILESTONE-001` | domain IDs/events, embedded store, daemon lifecycle, HTTP/SSE, CLI | restart/replay, idempotency, cursor ordering, backup/restore, auth, and CLI/API parity conformance pass |
| `KIT-MILESTONE-002` | agentkit loop/provider integration, prompt compiler, usage accounting | one prompt streams to completion through a durable run; intent/outcome ordering, cancellation, prompt digest, and core cost envelope pass |
| `KIT-MILESTONE-003` | sandboxed workspace/process ownership and cancellation | restricted process cannot escape filesystem/network/resource policy; whole-tree cancellation and outcome-unknown recovery pass |
| `KIT-MILESTONE-004` | lexical discovery, targeted reads, revisioned transactional edits, fast verification | Kit can inspect and safely modify its own repository through CLI/API; conflict, rollback, syntax, formatter, diagnostic, and diff evidence suites pass |
| `KIT-MILESTONE-005` | Tree-sitter, repository maps, LSP, optional graph/SCIP/semantic adapters, affected checks | core syntax/LSP localization beats lexical baseline within budget; each optional adapter ships only with incremental value and freshness evidence |
| `KIT-MILESTONE-006` | capability catalog, MCP broker, dynamic discovery, compact projections | search/inspect/bind/invoke, schema-digest pinning, list-change refresh, auth, and provider projection conformance pass |
| `KIT-MILESTONE-007` | compose Lua/Runlet, adaptive TOON, nested policy and tracing | direct/compose routing, nested authorization, cancellation, effect recording, and encoding ablations meet correctness gates |
| `KIT-MILESTONE-008` | model routing, batching, parallelism, background tasks, speculation | budgeted policies beat fixed serial baseline on a preregistered frontier without safety or tail-latency regression |
| `KIT-MILESTONE-009` | smart `yield` compaction and durable resume | checkpoint validation, post-mutation durability, state-transfer, context-pressure fallback, and resume suites pass |
| `KIT-MILESTONE-010` | ACP clients/subagents, A2A peers, simple observability web app | protocol conformance, remote lifecycle/cancellation, child workspace isolation, and public-API-only UI pass |
| `KIT-MILESTONE-011` | full benchmark, experiment, shadow, and canary infrastructure | reproducible harness, hidden/private corpus, statistical report, replay limits, and rollout guardrails pass |
| `KIT-MILESTONE-012` | clustered storage/executors and hostile multi-tenant isolation | lease/fencing failover, tenant isolation, disaster recovery, quota/fairness, and adversarial sandbox suites pass |

The product becomes **dogfoodable after `KIT-MILESTONE-004`**: a developer can use the daemon and CLI to run one agent against Kit itself, discover code lexically, apply conflict-safe edits, receive fast verification, inspect events/cost/diffs, cancel work, and recover after restart. Later slices improve intelligence, interoperability, optimization, and deployment strength without redefining that core loop.

This is an implementation ordering, not a reduction in product ambition. A slice is complete only when its registered `KIT-*` requirements have evidence; code presence or a demo is insufficient.

## 35. Open Research Questions

Kit should answer these experimentally rather than freezing assumptions:

- How short can the behavioral prompt become before repair turns rise?
- Which prompt rules generalize across model families, and which should be model-specific?
- What evidence and token budget maximize localization recall per token by language and repository size?
- When does an LSP/SCIP graph justify startup and maintenance cost over Tree-sitter plus lexical search?
- Which compact schema notation minimizes both prompt and hidden reasoning tokens?
- Can provider grammar constraints improve patch syntax without harming semantic quality?
- Which edit format is optimal per model and patch shape?
- Which fast checks best predict final suite success?
- Can expected-value-of-information policies reliably stop low-value reads and retries?
- When should composition route to direct calls, parallel calls, Lua, Runlet, or a deterministic host macro?
- Can Runlet familiarity be improved through fine-tuning or provider-side skills enough to erase its reasoning premium?
- Which payload classifier predicts when TOON beats compact JSON under each tokenizer?
- How should a model signal a checkpoint without spending an extra turn or prematurely ending useful work?
- Can continuation-packet quality be scored online without another expensive model?
- How much does shared prefix warming help real subagent fan-out under provider cache semantics?
- Which subagent decompositions improve verified results after counting merge and coordination cost?
- When does A2A delegation outperform local ACP subagents or direct tools?
- How should confidence be calibrated for abstention, escalation, and destructive effects?
- Which offline replay methods remain predictive after a policy changes its actions?

## 36. Decisions

This RFC makes the following foundational decisions:

1. Kit is a daemon and public runtime, not a client-centric application.
2. Agentkit supplies active loop primitives; Kit supplies durable product semantics.
3. Correctness and safety are gates; cost, tokens, and latency are Pareto objectives.
4. Source JSON Schema and dialect remain canonical per capability; compact notation is a one-way model projection.
5. Each protocol retains its native wire representation; TOON is adaptive model-context presentation only.
6. Repository discovery is hybrid and progressive, with syntax and semantic provenance preserved.
7. Editing is revisioned, staged, conflict-aware, and optionally verified in one call.
8. Dynamic tools use search, inspect, bind, and invoke rather than eager catalog stuffing.
9. Composition is task- and model-routed, not universally preferred.
10. ACP handles coding clients and subagents, A2A handles autonomous peers, and MCP handles capabilities.
11. The model may choose semantic checkpoint boundaries, but Kit validates summaries against authoritative state.
12. Every local process/agent and every remote task relationship is supervised, bounded, observable, and owned by the daemon.
13. The CLI and all clients use the same public command/query semantics.
14. Every optimization must survive task-level ablation and production-relevant evaluation.

## 37. References

Local case studies are primary evidence about the reviewed implementation but are self-authored, not independent validation. Product/vendor documentation establishes current interfaces, not causal quality gains. Mutable protocols, datasets, models, and harnesses MUST be pinned to the exact versions recorded by an experiment or build.

### Protocols and formats

- A2A 1.0.0 specification: <https://a2a-protocol.org/v1.0.0/specification/>
- A2A and MCP: <https://a2a-protocol.org/v1.0.0/topics/a2a-and-mcp/>
- ACP v1 overview: <https://agentclientprotocol.com/protocol/v1/overview>
- ACP repository: <https://github.com/agentclientprotocol/agent-client-protocol>
- MCP specification: <https://modelcontextprotocol.io/specification/2025-11-25/index>
- MCP tools: <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>
- MCP client scaling guidance: <https://modelcontextprotocol.io/docs/develop/clients/client-best-practices>
- TOON specification: <https://github.com/toon-format/spec/blob/main/SPEC.md>
- JSON Schema 2020-12: <https://json-schema.org/draft/2020-12>
- OpenAPI: <https://spec.openapis.org/oas/latest.html>
- RFC 9457 problem details: <https://www.rfc-editor.org/rfc/rfc9457.html>

### Agentkit and composition

- Local agentkit architecture: `../agentkit/docs/architecture.md`
- Local agentkit loop: `../agentkit/docs/loop.md`
- Local agentkit capabilities: `../agentkit/docs/capabilities.md`
- Local agentkit MCP: `../agentkit/docs/mcp.md`
- Local agentkit ACP: `../agentkit/docs/acp.md`
- Local agentkit compaction: `../agentkit/docs/compaction.md`
- Local compose study: `../agentkit/docs/compose-case-study.md`
- Local Runlet/TOON study: `../agentkit/docs/runlet-case-study.md`
- Runlet: <https://github.com/danielkov/runlet>

### Code intelligence and editing

- Tree-sitter: <https://tree-sitter.github.io/tree-sitter/>
- ast-grep: <https://ast-grep.github.io/guide/introduction.html>
- LSP specification: <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/>
- SCIP: <https://github.com/scip-code/scip/blob/main/scip.proto>
- Sourcegraph SCIP rationale: <https://sourcegraph.com/blog/announcing-scip>
- Aider repository map: <https://aider.chat/docs/repomap.html>
- Aider edit formats: <https://aider.chat/docs/more/edit-formats.html>
- SWE-agent ACI: <https://swe-agent.com/latest/background/aci/>
- SWE-agent paper: <https://arxiv.org/abs/2405.15793>
- Agentless: <https://arxiv.org/abs/2407.01489>
- AutoCodeRover: <https://arxiv.org/abs/2404.05427>
- RepoCoder: <https://arxiv.org/abs/2303.12570>
- LocAgent: <https://arxiv.org/abs/2503.09089>
- Repoformer selective retrieval: <https://arxiv.org/abs/2403.10059>
- CodePlan dependency-aware change planning: <https://arxiv.org/abs/2309.12499>
- ToolLLM/ToolBench: <https://arxiv.org/abs/2307.16789>

### Context, routing, and inference efficiency

- Anthropic context engineering: <https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents>
- Lost in the Middle: <https://arxiv.org/abs/2307.03172>
- OpenAI prompt caching: <https://platform.openai.com/docs/guides/prompt-caching>
- Anthropic prompt caching: <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
- vLLM automatic prefix caching: <https://docs.vllm.ai/en/latest/features/automatic_prefix_caching/>
- LLMCompiler: <https://arxiv.org/abs/2312.04511>
- RouteLLM: <https://arxiv.org/abs/2406.18665>
- FrugalGPT: <https://arxiv.org/abs/2305.05176>
- PagedAttention: <https://arxiv.org/abs/2309.06180>
- Speculative decoding: <https://arxiv.org/abs/2211.17192>
- LLMLingua: <https://arxiv.org/abs/2310.05736>
- SGLang and RadixAttention: <https://arxiv.org/abs/2312.07104>
- Orca iteration-level scheduling: <https://www.usenix.org/conference/osdi22/presentation/yu>
- DistServe prefill/decode disaggregation: <https://arxiv.org/abs/2401.09670>
- Sarathi-Serve chunked prefill: <https://arxiv.org/abs/2403.02310>
- EAGLE speculative decoding: <https://arxiv.org/abs/2401.15077>
- FlashAttention: <https://arxiv.org/abs/2205.14135>

### Evaluation

- SWE-bench: <https://arxiv.org/abs/2310.06770>
- SWE-bench Verified methodology: <https://openai.com/index/introducing-swe-bench-verified/>
- SWE-bench Multilingual: <https://www.swebench.com/multilingual.html>
- SWE-bench Live: <https://arxiv.org/abs/2505.23419>
- Terminal-Bench: <https://www.tbench.ai/>
- LiveCodeBench: <https://arxiv.org/abs/2403.07974>
- SWE-Lancer: <https://arxiv.org/abs/2502.12115>
- HumanEval and `pass@k`: <https://arxiv.org/abs/2107.03374>
- tau-bench and `pass^k`: <https://arxiv.org/abs/2406.12045>
- Anthropic agent evaluation guidance: <https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents>
- Predictive test selection: <https://arxiv.org/abs/1810.05286>
- METR software-task time horizons: <https://arxiv.org/abs/2503.14499>
- SPACE developer productivity: <https://doi.org/10.1145/3454122.3454124>
- DevEx framework: <https://doi.org/10.1145/3595878>

### Runtime, security, and observability

- Linux cgroup v2: <https://docs.kernel.org/admin-guide/cgroup-v2.html>
- Landlock Rust bindings: <https://landlock.io/rust-landlock/landlock/index.html>
- gVisor architecture: <https://gvisor.dev/docs/architecture_guide/intro/>
- Firecracker: <https://firecracker-microvm.github.io/>
- Git worktrees: <https://git-scm.com/docs/git-worktree>
- OpenTelemetry Rust: <https://opentelemetry.io/docs/languages/rust/>
- WHATWG server-sent events: <https://html.spec.whatwg.org/multipage/server-sent-events.html>
