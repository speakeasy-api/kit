# Kit Complete RFC Implementation Plan

- Status: Proposed execution plan
- Source: `RFC.md` at commit `f1893b56e9fba01fcf49c64c3cdf65dfdc7c253a`
- Source SHA-256: `7e36e05d308653526c27b17ff736579d4fe2cd5944f5c5cad5372a0b00303ba9`
- Scope: The entire initial complete product described by RFC 0001

## 1. Objective

Ship RFC 0001 in full. The program does not end at dogfoodability, a local-only product, protocol demos, or isolated benchmark wins. It ends when one release candidate implements the complete architecture, passes every applicable requirement and milestone gate, resolves every architectural promise and research-dependent default, and is operable in the local, restricted, clustered, and hostile multi-tenant deployment modes described by the RFC.

The plan preserves the RFC's twelve dependency-ordered milestones. Milestones are integration checkpoints, not permission to replace final semantics with temporary designs. A milestone can use a deliberately narrow implementation, but it cannot introduce a second source of truth, bypass path, or incompatible contract that later work must replace.

## 2. Completion Contract

RFC completion requires all of the following against the same release candidate:

1. Every normative statement has a stable `KIT-<AREA>-NNN` identifier.
2. Every normative requirement has current passing evidence of an allowed type: conformance test, evaluation, operational assertion, or explicit manual review.
3. Every testable non-normative architectural promise is registered and either implemented or resolved by an accepted experiment followed by an RFC amendment.
4. Every `MUST` and `MUST NOT` is implemented. It cannot be waived without changing the RFC.
5. Every `SHOULD` and `SHOULD NOT` is implemented unless evidence supports a documented exception and the RFC or requirement record captures it.
6. Every optional mechanism has an explicit applicability policy and a tested adapter contract. A concrete optional adapter ships only when its preregistered incremental-value gate selects it; otherwise its requirement closes as not selected with evidence and the conservative RFC fallback remains active. A named non-optional product capability cannot be silently omitted.
7. Every RFC §35 research question has a recorded answer, a supported policy, or a conservative disabled-by-default disposition backed by evidence.
8. Every risk in RFC §33 maps to implemented mitigation and verification evidence.
9. All twelve milestone gates pass together after clean installation, upgrade, restart, backup restore, and disaster recovery.
10. The requirement registry reports 100% normative evidence coverage, 100% architectural-promise resolution, 37/37 RFC section coverage, and zero open release-blocking findings.

Code presence, a demo, unit tests alone, or a historical milestone pass does not establish completion.

## 3. Program Rules

### 3.1 Architectural invariants

These rules apply from the first implementation commit:

- The semantic event log is the durable source of truth; agentkit loop state is not.
- The same command/query handlers serve HTTP, CLI, ACP, and first-party clients.
- The capability kernel introduced in M001 is the only authorization and invocation authority for direct, nested, local, and protocol capabilities; M006 extends it into the full broker without replacing it.
- Every externally visible effect has durable intent and outcome events or explicit `outcome_unknown`.
- Every active effect carries attempt ownership and fencing; stale owners cannot commit.
- Every process, terminal, workspace writer, child agent, and remote task relationship has one declared owner.
- Artifact bytes are durable and hash-verified before an event references them.
- Canonical structured data is stored separately from model-facing presentation.
- Repository facts, protocol metadata, tool descriptions, model output, and remote messages are data, never authority.
- All queues, streams, outputs, process trees, fan-out, spend, retries, and context are bounded.
- Unsupported security, isolation, diagnostic, protocol, or provider behavior fails closed or is reported unavailable; it is never approximated silently.
- Full authorized history remains lossless outside model context.

### 3.2 Implementation shape

Start as the RFC's one Rust binary and Cargo workspace with internal module boundaries matching `api`, `cli`, `domain`, `store`, `runtime`, `agent`, `capabilities`, `workspace`, `verify`, `protocols`, `executor`, `telemetry`, and `web`.

Do not create one crate per module by default. Split a crate only for one of these concrete reasons:

- a separate executable or sandbox boundary;
- an optional dependency or target platform boundary;
- an independently versioned protocol or storage adapter;
- a compile-time dependency cycle that cannot be removed cleanly;
- a test harness that must not link product internals.

Use one domain model, one error model, one authorization decision model, one event envelope, and one configuration materialization path. Adapters translate at boundaries; they do not create parallel lifecycle models.

### 3.3 Continuous evidence

The evaluation harness starts in milestone 0 and grows with each milestone. Phase 0 defines immutable trial manifests, M003 supplies isolated execution, and M004 supplies validated trials plus the minimum RFC §6 reporting/statistical engine required by measured M005-M008 gates. Milestone 11 broadens the corpus, statistics, replay, shadowing and rollout system; it does not introduce measurement for the first time.

Every work package must land with:

- linked requirement IDs;
- implementation and public-contract links;
- deterministic tests where behavior is deterministic;
- fault injection for durability, cancellation, and recovery boundaries;
- adversarial tests for authority and isolation boundaries;
- benchmark or experiment definitions for measured defaults;
- telemetry sufficient to reproduce its acceptance result;
- compatibility and migration notes when a persistent or public contract changes.

## 4. Phase 0: Make the RFC Executable

Phase 0 is required by RFC §5.1 and blocks product implementation.

### 4.1 Requirement registry

Create a machine-readable registry and generated human report. Use stable areas rather than section or milestone numbers:

| Prefix | Area |
| --- | --- |
| `KIT-GOV-` | governance, registration, evidence, tombstones |
| `KIT-OUTCOME-` | goals, metrics, optimization acceptance |
| `KIT-ARCH-` | module and authority boundaries |
| `KIT-AGENTKIT-` | agentkit integration and pinning |
| `KIT-DOMAIN-` | entities, lifecycle, ownership |
| `KIT-STORE-` | events, projections, artifacts, backup and recovery |
| `KIT-PROMPT-` | prompt compiler and behavioral policy |
| `KIT-CONTEXT-` | context projection, provenance and caching |
| `KIT-REPO-` | repository discovery and indexes |
| `KIT-TOOL-` | model-facing tools and schemas |
| `KIT-CAP-` | capability catalog, broker and binding |
| `KIT-COMPOSE-` | Lua/Runlet composition |
| `KIT-ENCODE-` | JSON, TOON and model presentation |
| `KIT-EDIT-` | edit IR, transactions and conflict handling |
| `KIT-VERIFY-` | checks, affected selection and feedback |
| `KIT-ROUTE-` | model and strategy policy |
| `KIT-RUNTIME-` | scheduling, cancellation, parallelism and recovery |
| `KIT-COMPACT-` | checkpoints and context replacement |
| `KIT-ACP-` | ACP client/server and subagent behavior |
| `KIT-A2A-` | A2A peers and remote tasks |
| `KIT-MCP-` | MCP lifecycle and adaptation |
| `KIT-SEC-` | authorization, approvals, secrets and SSRF |
| `KIT-EXEC-` | executors, processes, workspaces and terminals |
| `KIT-API-` | public API, streams, CLI and retention |
| `KIT-OBS-` | telemetry, health and observability UI |
| `KIT-EVAL-` | harness, statistics, experiments and rollout |
| `KIT-CONFIG-` | layered configuration and extensions |
| `KIT-VERSION-` | compatibility, schemas and dependency pins |
| `KIT-RELEASE-` | milestones and release gates |

Split compound prose and inherited lists into atomic records. The 61 lines containing normative keywords are only discovery input, not the final requirement count.

Each registry record contains:

```text
id, record_class, modality, title, atomic_text
source_section, source_anchor, source_quote, source_fingerprint
introduced_revision, status, supersedes, tombstone_reason
area, applicability, interpretation, acceptance_criteria
primary_milestone, contributing_milestones, dependencies, owner
criticality, platforms, deployment_tiers, release_gates
implementation_links, public_contract_links, telemetry_links
evidence_type, evidence_id, evidence_job, expected_result
artifact_digest, environment_digest, versions, latest_result
revalidation_rule, decision_record, deviation_record
```

Add CI that rejects:

- unregistered or changed normative text;
- duplicate or reused IDs;
- live requirements without owner, milestone, acceptance criteria, and evidence plan;
- tests and evaluations that cite unknown requirement IDs;
- tombstoned requirements without a replacement or decision record;
- release candidates with missing, stale, or failing evidence.

### 4.2 Architectural promises and decisions

Register testable declarative promises even when they lack normative keywords. Register RFC §36 decisions as architecture assertions and RFC §33 mitigations as evidence-bearing controls. Context, examples, observations, and references receive section coverage but do not inflate the implementation denominator.

### 4.3 Repository and build foundation

Create the smallest production-capable workspace:

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
src/
  api/ cli/ domain/ store/ runtime/ agent/ capabilities/
  workspace/ verify/ protocols/ executor/ telemetry/ web/
tests/
  conformance/ fault/ adversarial/ integration/
eval/
  manifests/ graders/ reports/
requirements/
  registry.yaml evidence.yaml tombstones.yaml
docs/
  decisions/ operations/ compatibility/
```

Pin Rust, agentkit commit/tree digest and features, Runlet, protocol revisions, schema dialects, TOON, grammars, harnesses, and initial build images. Generate a build manifest containing those pins.

Define the immutable task, trial, environment, budget, cache-condition, grader and outcome manifest schemas here. They become the single experiment identity used by component benchmarks and the later M011 platform.

Establish CI lanes for formatting, linting, unit/integration tests, requirement lint, schema compatibility, fault tests, adversarial tests, reproducible builds, dependency licenses, vulnerability scanning, and evidence reports.

### 4.4 Threat and fault model

Document trust boundaries and abuse cases for:

- local and remote API ingress;
- state root, SQLite and artifacts;
- workspace mutation and hostile repositories;
- local, container and VM execution;
- model/provider calls and prompt injection;
- native, MCP and composed capabilities;
- ACP children and A2A peers;
- secrets, URLs, redirects and egress;
- telemetry, retention, backups and deletion;
- clustered control plane and executors.

Build reusable fake providers, malicious repositories, MCP/ACP/A2A simulators, clock and lease controls, crash points, storage fault injection, and sandbox capability probes.

### 4.5 Phase 0 exit

- All RFC normative statements and architectural promises are registered atomically.
- Every record has milestone ownership and an evidence plan.
- Requirement lint passes and protects future RFC edits.
- Reproducible empty product builds pass on supported development platforms.
- Threat model and fault matrix cover every authority and persistence boundary.
- Versioned evaluation manifests validate and can identify an empty trial before product execution exists.
- The twelve milestone evidence dashboards exist, even though product evidence is initially pending.

## 5. Delivery Graph and Parallel Work

### 5.1 Critical path

```text
Phase 0
  -> M001 durable control plane
      +-> M002 durable agent execution --+
      +-> M003 isolated execution -------+-> M004 safe core coding loop
```

M004 is the trusted-local dogfood boundary.

### 5.2 Full dependency graph

Work may start before a milestone's complete exit dependency set is green when it consumes only stable earlier contracts. The early-start graph is:

```text
M001 -> M002
M001 -> M003
M001 + M002 -> early M006 and M009 work
M001 + M003 -> early M012 store/executor work
M002 + M003 -> M004
M002 + M003 -> early M008 policy/scheduler work
Phase 0 -> continuous M011 harness work
```

Milestone exit gates use the stricter DAG below:

```text
Phase 0 -> M001
M001 -> M002
M001 -> M003
M002 + M003 -> M004
M004 -> M005
M001 + M002 + M003 + M004 -> M006
M006 -> M007
M005 + M007 -> M008
M002 + M004 + M005 + M006 -> M009
M001 + M003 + M004 + M006 + M008 + M009 -> M010
M002 + M003 + M004 + M005 + M006 + M007 + M008 + M009 + M010 -> M011
M001 + M003 + M008 + M010 + M011 -> M012
M005 + M007 + M008 + M009 + M010 + M011 + M012 -> final complete join
```

The M011 harness track runs continuously from Phase 0. PostgreSQL/object-store and hostile-executor prototypes may begin after M001/M003 contracts stabilize, but M012 cannot release before M011 proves the complete topology.

### 5.3 Long-running workstreams

| Workstream | Begins | Primary outputs |
| --- | --- | --- |
| Governance and evidence | Phase 0 | registry, conformance map, release reports |
| Domain, storage and API | M001 | lifecycle, event store, artifacts, API/CLI |
| Agent and context | M002 | agentkit loop, prompts, model calls, accounting |
| Execution security | M003 | workspaces, sandboxes, processes, terminals |
| Coding intelligence | M004 | search, edit, verification, indexes |
| Capability ecosystem | M006 | broker, schemas, MCP, composition |
| Policy and optimization | M008 | routing, scheduling, speculation, caching |
| Protocol ecosystem | M010 | ACP, A2A, MCP server, public clients |
| Evaluation and release | Phase 0 | harness, experiments, canaries, final evidence |
| Clustered operations | M012 preparation | PostgreSQL, object store, remote executors, hostile tier |

### 5.4 Cross-milestone foundations

The following foundations have explicit early delivery points so later milestone gates do not depend on ad hoc substitutes:

| Foundation | Delivery | Consumers |
| --- | --- | --- |
| Evaluation manifests | Phase 0 | every benchmark and experiment |
| Isolated trial execution | M003-W11 | M004-W12, M005, M007, M008, M011 |
| Harness validation and core statistics | M004-W12 | all measured M005-M008 gates |
| Layered effective configuration | M001-W05 | all runs, adapters, experiments and authorization decisions |
| Capability/security kernel | M001-W06-W08 | M002 model/tools, M004 native tools, M006 broker, M007 composition |
| Bounded safety scheduler | M001-W04 | every model, tool, process and background task |

Configuration extension ownership is cumulative rather than deferred to a generic final audit:

| Milestone | Configuration and extension responsibility |
| --- | --- |
| M001 | built-in/user/project/run/experiment precedence, provenance, canonical materialization and authority intersection |
| M002 | model, prompt, context, provider and cost-table adapters |
| M005 | repository language/index, edit and verification adapters |
| M006 | native capability providers, MCP and schema/projection adapters |
| M008 | router, scheduler and experiment-policy adapters |
| M010 | ACP/A2A clients, peers and skill exposure |
| M012 | store, executor, tenant and deployment adapters |

Each owner adds schema validation, compatibility pins, public documentation and conformance tests. In-process extensions are explicitly trusted; untrusted extensions use the sandboxed out-of-process protocols required by RFC §31.

## 6. KIT-MILESTONE-001: Durable Control Plane

### 6.1 Outcome

A restart-safe single-binary daemon whose domain, event, API, authorization, and CLI contracts are final enough for every later subsystem to use without bypasses.

### 6.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M001-W01` | Opaque typed IDs; all RFC entities; versioned commands, events and projections; run/attempt lifecycle; CAS transitions; ownership and fencing fields | Phase 0 |
| `M001-W02` | SQLite WAL event store with atomic multi-stream append, expected versions, idempotency records, commit positions, projections, migrations and authoritative store time | W01 |
| `M001-W03` | Retention, legal-hold, shared-reachability and backup-expiry model used by artifacts, deletion and backup inventory | W01-W02 |
| `M001-W04` | Safety scheduler kernel: immutable run budgets, atomic spend reservation/debit, turn/tool/process limits, bounded admission queues, principal/global concurrency caps and deterministic exhaustion | W01-W02 |
| `M001-W05` | Schema-versioned built-in/user/project/run/experiment configuration layering, field provenance, canonical materialization, grant intersection and immutable run snapshot | W01 |
| `M001-W06` | Security substrate: opaque secret handles, sensitive-field classification, capture-boundary redaction, safe structured logging, URL/redirect/rebinding policy and egress authorization | W01-W02 |
| `M001-W07` | Authentication/authorization contract plus local peer credentials, loopback bearer/Origin/Host checks and remote mTLS/OIDC validation | W01, W05-W06 |
| `M001-W08` | Capability kernel: immutable identity, preserved source schema/digest, grant decision, invocation envelope, intent/outcome, budget, approval, cancellation and accounting | W01-W07 |
| `M001-W09` | Content-addressed artifact store with write/sync/hash-before-reference, manifests, policy-aware reachability and orphan GC | W02-W03, W06-W07 |
| `M001-W10` | Leases, monotonic fencing counters, state-root lock, startup reconciliation and graceful shutdown | W01-W04, W09 |
| `M001-W11` | Command/query service; versioned authenticated OpenAPI HTTP API; RFC 9457 errors; authorized SSE cursors; terminal WebSocket reservation | W01-W10 |
| `M001-W12` | Thin CLI using the command/query service, human/JSON/JSONL output, daemon discovery, readiness and optional auto-start | W11 |
| `M001-W13` | Consistent SQLite backups plus artifact manifests and policy metadata, multiple generations outside state root, automated restore verification and backup health | W03, W09-W10 |
| `M001-W14` | Archive, asynchronous deletion and retention APIs backed by legal hold, shared reachability, backup inventory and earliest physical-deletion time | W03, W09-W11, W13 |
| `M001-W15` | Versioned OpenTelemetry-compatible traces/metrics/log adapter, core run envelope, cardinality controls, redaction, local encryption/retention hooks, liveness/readiness and evidence report | W01-W14 |

Store/artifact, configuration/security, scheduler and transport clients can proceed in parallel after W01-W02. No listener becomes ready before W07 authorization is installed. Deletion and backup share W03 and are tested together.

### 6.3 Exit evidence

- Concurrent append tests prove unique ordered positions and no skipped committed prefix.
- Restart/replay produces identical projections.
- Idempotency replay, pending replay, retention and conflicting digest behavior match the API contract.
- Configuration precedence, provenance, digest stability and restart determinism pass; later layers cannot expand authenticated grants.
- Budget reservation cannot overspend or leak across crash, retry or cancellation, and every admission queue is bounded.
- Native capability-kernel calls cannot bypass grant, intent/outcome, budget, approval, cancellation or accounting.
- Stale versions and stale fences cannot commit.
- Artifact crash points never produce referenced missing bytes.
- Slow event consumers reconnect from durable cursors or receive explicit cursor expiry and a projection snapshot.
- Cross-principal API, event and artifact access is denied without information leakage.
- Backup restore into a fresh state root passes integrity and projection checks; deleted shared/unshared content remains restorable only until the advertised backup-expiry boundary.
- Authentication tests cover unauthenticated, cross-principal, Origin/Host, token replay, expiry and revocation cases before readiness.
- Telemetry exports RFC spans without high-cardinality metric labels and encrypts/redacts retained sensitive surfaces according to policy.
- CLI/API parity is generated or tested for every exposed command and query.
- All applicable `KIT-DOMAIN`, `KIT-STORE`, `KIT-API`, `KIT-CAP`, `KIT-CONFIG`, `KIT-RUNTIME`, `KIT-SEC`, `KIT-OBS` and `KIT-RELEASE` evidence is green.

### 6.4 RFC coverage

Primary: §§5, 8, 10, 27. Supporting: §§3, 6, 7, 28, 31-34, 36-37.

## 7. KIT-MILESTONE-002: Durable Agent Execution

### 7.1 Outcome

One prompt can stream through a durable run using agentkit while Kit remains the source of truth for intent, outcome, interruption, transcript, prompt, and cost.

### 7.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M002-W01` | Pin and audit agentkit; define Kit-to-agentkit item, interrupt, cancellation and usage mapping | M001 |
| `M002-W02` | One `LoopDriver` per attempt; complete restart from non-compacted safe boundaries and waiting states using committed events/snapshots | W01, M001-W10 |
| `M002-W03` | Durable model and tool adapters invoking the M001 capability/security/scheduler kernels and committing intent before dispatch and outcome or `outcome_unknown` before loop return | W02, M001-W04-W08 |
| `M002-W04` | Versioned prompt compiler with stable/dynamic modules, canonical serialization, default behavior policy and task contract from the immutable effective configuration | W01, M001-W05 |
| `M002-W05` | Context projection with provenance, revision, retrieval reason, token estimate, artifact handles and deterministic budgets | W04 |
| `M002-W06` | Provider adapters, streaming deltas, safe-boundary retry, input/approval/auth interruption and model cancellation | W02-W03 |
| `M002-W07` | Usage and cost accounting by input/cache/reasoning/output/tool/compute category, null for unavailable fields | W03-W06 |
| `M002-W08` | Prompt/cache digest, divergence, model snapshot, effective configuration digest and core run telemetry | W04-W07 |
| `M002-W09` | Versioned model, provider, prompt-module and cost-table extension contracts with trusted in-process boundary | W04-W08, M001-W05 |

Prompt/context, provider integration, and accounting can proceed in parallel against fake providers.

### 7.3 Exit evidence

- A public API and CLI prompt reaches durable completion and streams committed progress.
- Crash injection around each intent, dispatch and outcome boundary never duplicates an effect or invents success.
- Every non-compacted safe boundary and waiting state resumes after restart; in-flight uncertainty is explicit and non-idempotent work is not blindly repeated.
- Spend, turn and call reservations remain bounded and are released or reconciled across crash, retry and cancellation.
- Prompt compilation is byte-deterministic and excludes timestamps/run IDs from stable prefixes.
- Prefix and usage categories reconcile with provider fixtures; unavailable values remain unavailable.
- Blocking approval/auth/input survives restart and resolves through authenticated domain commands.
- Hidden chain-of-thought is absent from events, artifacts, logs and traces.
- Canary credentials placed in provider headers, errors and streamed content are absent from prompts, events, artifacts, traces and logs.
- Applicable `KIT-AGENTKIT`, `KIT-PROMPT`, `KIT-CONTEXT`, `KIT-CONFIG`, `KIT-CAP`, `KIT-RUNTIME`, `KIT-SEC`, `KIT-OUTCOME` and `KIT-OBS` evidence is green.

### 7.4 RFC coverage

Primary: §§9, 11, 12. Supporting: §§6-8, 10, 14, 19-20, 22, 28-29, 31-32.

## 8. KIT-MILESTONE-003: Isolated Workspaces and Processes

### 8.1 Outcome

All repository code, tools, language servers, package managers and child processes execute inside an owned, bounded and cancellable executor profile rather than on the daemon host by default.

### 8.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M003-W01` | Executor profile covering trust tier, mounts, source-write mode, credentials, egress, resources and fail-closed requirements | M001 |
| `M003-W02` | Git worktree/clone/COW acquisition, base and dirty-state hashes, separate writer snapshots and safe cleanup | W01 |
| `M003-W03` | Trusted-local OS sandbox and explicit weaker compatibility mode | W01-W02 |
| `M003-W04` | Restricted rootless container/OS sandbox with read-only source, writable build/temp, deny-by-default network and resource limits | W01-W02 |
| `M003-W05` | Process ownership, pipes, bounded output, process-tree lifecycle, daemon-service ownership and whole-tree reaping | W03-W04 |
| `M003-W06` | PTY allocation, one input-writer lease, read-only viewers, sequencing, resize and retention | W05 |
| `M003-W07` | Durable cancellation, grace/kill/reap sequence, executor quiescence confirmation and crash reconciliation | W05-W06 |
| `M003-W08` | Executor integration for M001 secret handles/redaction plus file-descriptor, memory-file and scoped environment injection; hooks/submodules disabled or sandboxed | W03-W05, M001-W06 |
| `M003-W09` | Mutation-overlay contract consumed by M004 and isolated-VM interface consumed by M012 | W02-W05 |
| `M003-W10` | Authorized process, terminal and terminal-attachment API/OpenAPI/event surfaces plus CLI/SDK coverage | W05-W07, M001-W11 |
| `M003-W11` | Fresh isolated evaluation-trial executor consuming Phase 0 manifests, with hidden grader/gold mounts outside agent authority | W04-W09 |

Platform backends, workspace fixtures and process supervision can proceed in parallel behind W01.

### 8.3 Exit evidence

- Adversarial filesystem, symlink, hard-link, traversal, mount, socket, metadata-service and network escape suites pass.
- CPU, memory, PID, file, disk, I/O, output and wall-time bounds are enforced.
- Forked descendants die and are reaped after cancellation or daemon failure.
- Required unavailable isolation rejects execution; host mode is never mislabeled as isolation.
- Workspace records and preserves pre-existing user changes.
- Stale attempts and escaped-process simulations cannot publish effects or receive reassigned workspaces.
- Secrets do not appear in inherited environment, argv by default, output artifacts, terminal retention or traces.
- Process and terminal API resources enforce ownership, viewer/writer leases, retention and CLI/SDK parity.
- Trial fixtures cannot read or mutate hidden graders, gold patches or acceptance rules.
- Applicable `KIT-EXEC`, `KIT-RUNTIME`, `KIT-SEC`, `KIT-DOMAIN`, `KIT-STORE`, `KIT-API` and `KIT-EVAL` evidence is green.

### 8.4 RFC coverage

Primary: §§25-26. Supporting: §§10, 18, 21, 23-24, 28, 31-33.

## 9. KIT-MILESTONE-004: Safe Core Coding Loop

### 9.1 Outcome

Kit can inspect and modify its own repository through public API/CLI/agent surfaces with conflict-safe transactional edits and fast verification. This is the dogfood boundary, not the final product boundary.

### 9.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M004-W01` | Managed workspace revisions, content hashes, watcher invalidation and periodic reconciliation | M003 |
| `M004-W02` | `.gitignore`-aware metadata, bounded lexical search, ranked discover, focused read, artifacts and revision-scoped cursors | W01 |
| `M004-W03` | Canonical `AddFile`, `DeleteFile`, `MoveFile`, `ReplaceRange` IR and model-format normalizers | W01 |
| `M004-W04` | Root-relative path authorization using pre-opened handles and no-follow component checks | M003, W03 |
| `M004-W05` | Mutation lock, revision/base/anchor validation, complete parse, COW staging, syntax and formatter adapters | W03-W04 |
| `M004-W06` | Recovery manifest, undo images, synced materialization, revision event, startup roll-forward/rollback and actual diff artifact | W05 |
| `M004-W07` | `none`, `syntax`, `fast`, explicit `targeted` and explicit `full` profiles with `commit|abort` failure behavior | W05-W06 |
| `M004-W08` | Red-baseline diagnostics, bounded failure feedback, full log artifacts and check events | W07 |
| `M004-W09` | Provider grammar-constrained edit-output adapter behind the same parser/validator and an explicit experiment flag | M002, W03-W08 |
| `M004-W10` | Native `discover`, `search`, `read`, `edit`, `run`, `check` only through the M001 capability kernel | W02, W06-W08, M001-W08 |
| `M004-W11` | Public API, CLI and agent integration plus Kit-on-Kit dogfood scenario | W10 |
| `M004-W12` | Evaluation foundation: harness self-validation, reproducible trial execution, core RFC §6 metrics, preregistration and paired confidence/non-inferiority analysis | M003-W11, W11 |

Discovery, edit engine and verification adapters can proceed in parallel once revision semantics are fixed.

### 9.3 Exit evidence

- Kit completes a real self-change through the public surfaces and returns events, cost, diff and verification evidence.
- Crash injection at every manifest state proves rollback before revision commit and roll-forward after it.
- Duplicate anchors, stale revisions, external edits, Unicode, CRLF, missing newline, binary and symlink cases are safe.
- Cancellation before and after prepare obeys recovery semantics.
- Formatters cannot accidentally commit undeclared files or unrelated check outputs.
- `commit|abort` and diagnostic-delta behavior matches every verification profile.
- Grammar-constrained and ordinary edit outputs enter the identical validation and transaction path; the constrained path stays disabled until evaluated.
- No native tool bypasses the capability kernel's authorization, ownership, intent/outcome, budgets, events, artifacts or bounded output.
- Original/reference/empty/malformed/adversarial harness cases validate, and component trials emit the versioned core statistical report consumed by M005-M008.
- Applicable `KIT-REPO`, `KIT-TOOL`, `KIT-EDIT`, `KIT-VERIFY`, `KIT-EXEC`, `KIT-API`, `KIT-CAP`, `KIT-SEC` and `KIT-EVAL` evidence is green.

### 9.4 RFC coverage

Primary: §§13.1, 13.5-13.7, 14.1-14.2, 18, 19.1, 19.3. Supporting: §§2-3, 7, 10, 25-29, 33-34.

## 10. KIT-MILESTONE-005: Repository Intelligence and Affected Verification

### 10.1 Outcome

The core syntax/LSP discovery path beats the lexical baseline under equal budgets, all repository facts retain provenance and freshness, and affected verification remains bounded by deterministic safety floors.

### 10.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M005-W01` | Incremental Tree-sitter parsing keyed by grammar ABI/query hash; canonical syntactic symbol records | M004 |
| `M005-W02` | `ast-grep` structural search and previewable rewrites normalized through the edit IR | W01 |
| `M005-W03` | Long-lived LSP daemon services, document-version fencing, semantic facts, diagnostics and `WorkspaceEdit` normalization | M003-M004 |
| `M005-W04` | Isolated shadow LSP document/workspace diagnostics with explicit per-server fallback | W03 |
| `M005-W05` | Personalized token-budgeted repository map and bounded relationship expansion | W01-W03 |
| `M005-W06` | Package/file/symbol/test graph and Git history, blame, co-change and confidence/provenance edges | W01-W05 |
| `M005-W07` | Preregistered core retrieval corpus, oracle localization, primary estimand by language/repository class and correctness/latency guardrails | W01-W06, M004-W12 |
| `M005-W08` | Conditional SCIP and derived code-graph adapters over commit indexes plus live overlays | W06-W07 |
| `M005-W09` | Conditional sparse and dense semantic retrieval adapters with revision and provenance contracts | W05-W07 |
| `M005-W10` | Affected-check selector using core policy, changed paths, build/package graph, symbols, history, coverage and validated model proposals | W05-W07 |
| `M005-W11` | Incremental-value trials and enablement policy for selected optional adapters through the common evidence interface; not-selected dispositions for the rest | W07, W10, selected W08-W09, M004-W12 |
| `M005-W12` | Versioned language/index, edit and verification adapter contracts with configuration and compatibility conformance | W01-W11, M001-W05 |

Tree-sitter/ast-grep, LSP and history/relationship work are parallel tracks. W08-W09 begin only for cells whose preregistered value-of-information threshold justifies implementation; unselected adapters remain optional dependencies outside the default binary. W10 consumes their common evidence interface when enabled but does not depend on them.

### 10.3 Exit evidence

- Core syntax/LSP localization meets its preregistered primary estimand for each supported class, with correctness, provenance, token, freshness, latency and downstream-resolution guardrails.
- Every fact reports revision, range, source and confidence; syntax facts never masquerade as semantic references.
- Watcher loss, stale index, old LSP version and working-tree overlay races return fresh results or explicit staleness.
- Maps and traversals obey item, token, hop and degree bounds.
- Shadow staged content never reaches the live LSP session.
- Critical and explicit checks cannot be removed by the learned/model selector.
- SCIP, derived graph, sparse and dense adapters each have incremental-value evidence and conformance tests when selected, or a registered not-selected disposition that leaves the deterministic core fallback active.
- Applicable `KIT-REPO`, `KIT-EDIT`, `KIT-VERIFY`, `KIT-CONFIG`, `KIT-VERSION`, `KIT-RUNTIME`, `KIT-OUTCOME` and `KIT-EVAL` evidence is green.

### 10.4 RFC coverage

Primary: §13 and §19.2. Supporting: §§7, 12, 14, 18-21, 29-30, 33-35.

## 11. KIT-MILESTONE-006: Capability Catalog and MCP Broker

### 11.1 Outcome

Native and external capabilities share one secure catalog, immutable schema binding and invocation path, while large catalogs stay outside model context until authorized discovery.

### 11.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M006-W01` | Extend the M001 schema kernel with normalized forms and provider/model projections while preserving source dialect, documentation and digests | M001-W08, M002 |
| `M006-W02` | Extend grants with catalog arguments, effect classes, egress, credential and delegation-depth constraints without creating a second decision model | M001-W05-W08, M003 |
| `M006-W03` | Extend the sole capability kernel into the full broker for external/dynamic/nested calls, durable auth interrupts and provider accounting | W01-W02, M002 |
| `M006-W04` | Federated catalog identity, trust/source metadata, availability, side effects, reliability, latency and cost | W01-W03 |
| `M006-W05` | Authorization-filtered search/inspect/bind/invoke; immutable authorization snapshot and schema-digest binding | W04 |
| `M006-W06` | Provider deferred registration when supported and generic eager `tools.invoke` fallback | W05 |
| `M006-W07` | MCP stdio and Streamable HTTP clients; tools/resources/prompts; auth resume; list-change coalescing; sampling/elicitation/roots responders | W03-W05 |
| `M006-W08` | Apply and harden the M001 URL/egress substrate for MCP discovery, redirects, DNS changes and server-originated URLs | W02-W07, M001-W06 |
| `M006-W09` | Tool-learning telemetry for opportunities, search, inspection, call, errors and outcomes | W04-W07 |
| `M006-W10` | Canonical structured results separate from model presentation and nested-call fields required by M007 | W03 |
| `M006-W11` | Extension registry and compatibility contracts for native providers, MCP servers and schema/projection adapters; out-of-process enforcement for untrusted extensions | W01-W08, M001-W05 |

Schema/projection, broker/security, catalog and MCP transport work can proceed in parallel around stable W01-W03 contracts.

### 11.3 Exit evidence

- Unauthorized capabilities are absent from search and inspection, not merely blocked at invocation.
- Schema round-trip and provider projection tests reject unsupported constraints rather than dropping them.
- Existing bindings remain immutable through schema and list-change races; changed grants expire them.
- Direct and generic invocation produce equivalent policy, events, traces and results.
- MCP restart, auth interruption, list-change storms and malformed server behavior are bounded and recoverable.
- Secret and prompt-injection suites cover prompts, events, traces, composition inputs, terminal history and workspace metadata.
- URL adversarial tests cover redirects, rebinding, local services and unauthorized destinations.
- Applicable `KIT-TOOL`, `KIT-CAP`, `KIT-MCP`, `KIT-SEC`, `KIT-VERSION` and `KIT-OBS` evidence is green.

### 11.4 RFC coverage

Primary: §§14.3, 15, 23.5, 24. Supporting: §§8-9, 16-17, 28-32, 33-35.

## 12. KIT-MILESTONE-007: Composition and Adaptive Encoding

### 12.1 Outcome

Direct, parallel, deterministic host-macro, Lua and Runlet execution are production-safe strategies over the same broker, and model presentation can use TOON or compact alternatives without changing canonical semantics.

### 12.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M007-W01` | Nested broker invocation preserving principal, binding, grant, trace, idempotency and remaining budgets | M006 |
| `M007-W02` | Composition budget model for calls, fan-out, concurrency, CPU, memory, output, wall time, retries and effect classes | W01 |
| `M007-W03` | Broker-mediated bounded parallel direct calls with structured join/cancellation, deterministic result ordering, partial-outcome recording and shared budget enforcement | W01-W02 |
| `M007-W04` | Sandboxed Lua 5.4 backend with cancellation, partial-effect and artifact recording | W01-W02 |
| `M007-W05` | Pinned Runlet backend with schema checking, structured concurrency and repairable located diagnostics | W01-W02 |
| `M007-W06` | Deterministic host-macro backend for approved repeated high-value sequences, generated or authored outside model execution | W01-W02 |
| `M007-W07` | Bounded category approvals and explicit non-transactional remote-effect semantics | W01-W06 |
| `M007-W08` | Canonical JSON plus compact JSON, TOON WD 3.3, text, table and artifact-handle presentations | M006, W01 |
| `M007-W09` | Shape/tokenizer eligibility, strict TOON decoding, width/length limits, injection treatment and escaping | W08 |
| `M007-W10` | Strategy and encoding policy with deterministic fallback; version and decision in effective run configuration | W03-W09 |
| `M007-W11` | CI-compiled exemplars and preregistered paired composition/encoding experiments using the M004 evaluation foundation | W03-W10, M004-W12 |

Lua, Runlet, nested security and encoding can proceed in parallel after W01-W02.

### 12.3 Exit evidence

- Direct and nested calls have identical validation, authorization, approval, secret, cancellation and accounting behavior.
- Parallel calls obey fan-out/concurrency limits, cancel and join as a structured group, retain every partial outcome and return deterministic ordering.
- Crash/cancel/timeout tests never turn partial effects into success or blind retry.
- Canonical JSON round-trips independently of presentation.
- Malformed and adversarial composition/TOON inputs remain bounded and untrusted.
- Paired repeated trials compare direct, parallel, host macro, Lua and Runlet and establish allowed `(model, task shape, risk)` strategy and payload cells under RFC §6.2.
- Every documented program compiles in CI against pinned schemas and runtimes.
- Applicable `KIT-COMPOSE`, `KIT-ENCODE`, `KIT-CAP`, `KIT-SEC`, `KIT-OUTCOME` and `KIT-EVAL` evidence is green.

### 12.4 RFC coverage

Primary: §§16-17. Supporting: §§6-7, 9, 14-15, 24, 28-35.

## 13. KIT-MILESTONE-008: Routing, Scheduling and Speculation

### 13.1 Outcome

A bounded policy runtime improves the complete trajectory over a fixed serial baseline without weakening correctness, authority, reliability or interactive tail latency.

### 13.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M008-W01` | Versioned router features for task, repository, difficulty, ambiguity, risk, context, tools, health, cache, SLO and failures | M002, M005-M007 |
| `M008-W02` | Deterministic model, reasoning, verbosity, tool, verification, escalation, abstention and stop policies with safety floors | W01 |
| `M008-W03` | Extend the M001 safety scheduler with adaptive weighted fairness, provider admission, interactive/background classes and subagent-depth limits | M001-W04, M003 |
| `M008-W04` | Extend M007's structured parallel primitive to batching and independent model/search/check work | W02-W03, M007-W03 |
| `M008-W05` | Failure signatures, duplicate-hypothesis suppression and repeated-failure re-localization/escalation | W01-W04 |
| `M008-W06` | Isolated speculative work with expected-value decision, quotas, cancellation and wasted-spend accounting | M003-M004, W03-W05 |
| `M008-W07` | Revision/authorization-keyed deterministic-read memoization, provider predicted-output policy, dependency/compiler/build/test-discovery caches and offline batch APIs | W01-W04 |
| `M008-W08` | Cache-aware affinity, provider health, prefix warming, warm mirrors/images and persistent safe connections | W01-W07 |
| `M008-W09` | Expected-value-of-information ranking for reads/tests and policy-gated fresh-context review | W01-W08, M005 |
| `M008-W10` | Logged policy decisions, shadow policies, sticky assignments and bounded rollback hooks | W01-W09 |
| `M008-W11` | Preregistered fixed-serial versus routed-policy experiment and load/fairness suite using the M004 evaluation foundation | W01-W10, M004-W12 |
| `M008-W12` | Versioned router, scheduler and experiment-policy extension contracts consuming immutable effective configuration | W01-W11, M001-W05 |

Router, scheduler, provider/cache work and workload simulation are parallel tracks.

### 13.3 Exit evidence

- Full-trajectory comparison includes failures, speculative waste, verification and human intervention.
- Routed policy moves the accepted Pareto frontier under RFC §6.2.
- Interactive p95/p99 remains within SLO during indexing, evaluations and subagent pressure.
- Principal/provider/bulk queues make progress without starvation.
- Cancellation and outage release reservations and recover queues safely.
- Memoization, predicted output, build/dependency caches, batch lanes, evidence ranking and fresh-context review each preserve revision, authorization and safety semantics and remain off when their acceptance gate fails.
- Missing model features or policy services use deterministic fallbacks.
- No learned policy grants authority or suppresses mandatory safety floors.
- Applicable `KIT-ROUTE`, `KIT-RUNTIME`, `KIT-CONFIG`, `KIT-VERSION`, `KIT-OUTCOME`, `KIT-SEC`, `KIT-OBS` and `KIT-EVAL` evidence is green.

### 13.4 RFC coverage

Primary: §§20-21, 26, 29. Supporting: §§6-7, 10-12, 19, 24-25, 28, 30-35.

## 14. KIT-MILESTONE-009: Smart Compaction and Durable Resume

### 14.1 Outcome

Kit preserves lossless external history while replacing model context only at validated semantic boundaries and reconstructing attempts safely after restart.

### 14.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M009-W01` | Add, review and reproducibly pin agentkit's fallible post-validation checkpoint hook; upstream submission is tracked but does not block Kit | M002 |
| `M009-W02` | Checkpoint candidate, validated, rejected and promoted event/artifact states plus safe-boundary contract | M001-M002, W01 |
| `M009-W03` | Deterministic eviction of reasoning parts, stale logs, duplicate reads, superseded maps and successful command noise | M002, W02 |
| `M009-W04` | Typed `yield` continuation packet and sole-call/no-in-flight enforcement | M006, W02 |
| `M009-W05` | Authoritative enrichment and validation from task, workspace, verification, artifacts and operation state | M004-M006, W04 |
| `M009-W06` | Atomic promotion of validated transcript projection before the next model call | W01-W05 |
| `M009-W07` | Extend M002 restart reconstruction to validated checkpoint projections and add handle-based historical retrieval | M002, W06 |
| `M009-W08` | Threshold/budget policy, repeated-rejection fallback and cache-interaction telemetry | W03-W07 |
| `M009-W09` | State-transfer corpus, crash matrix and structural-versus-semantic trials | W03-W08 |

The agentkit hook, persistence/validator, deterministic compaction and evaluation corpus can proceed in parallel. Semantic compaction cannot enable until W01 and W06 pass.

### 14.3 Exit evidence

- Crash injection proves only validated committed checkpoints resume.
- Missing requirements, stale revisions, false check claims, missing artifacts and invalid tool/result pairs reject without context mutation.
- Mixed or mid-flight `yield` calls return retryable errors and do not alter context.
- State-transfer trials retain every requirement, changed file, failure, rejected approach, decision and next action.
- Validated checkpoint boundaries resume after daemon restart while retaining M002's non-compacted safe-boundary semantics; unsupported in-flight work remains explicit error or `outcome_unknown`.
- Deterministic fallback prevents provider context overflow.
- Semantic/model-selected compaction passes correctness non-inferiority and reports tokens, cache, cost and repair effects.
- Applicable `KIT-COMPACT`, `KIT-AGENTKIT`, `KIT-CONTEXT`, `KIT-STORE`, `KIT-RUNTIME` and `KIT-EVAL` evidence is green.

### 14.4 RFC coverage

Primary: §22. Supporting: §§7, 9-12, 19, 26, 28-30, 32-35.

## 15. KIT-MILESTONE-010: Protocol Ecosystem and Observability UI

### 15.1 Outcome

ACP clients and subagents, A2A peers, complete MCP roles and the built-in observability application operate through durable Kit semantics without flattening protocol lifecycles.

### 15.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M010-W01` | Final `AgentLink` and `ExternalTask` state machines, parent/child grants, budgets, workspaces, artifacts and cancellation | M001, M003-M006 |
| `M010-W02` | ACP v1 server mapping sessions to Kit threads/runs, streaming, permissions, filesystem, terminal and negotiated `session/load` | W01, M009 |
| `M010-W03` | ACP child client/supervisor with local spawn, heartbeat, reconnect/load, narrow grants, isolated snapshots and explicit merge | W01-W02 |
| `M010-W04` | A2A 1.0.0 outbound delegation and inbound Agent Card skills over a pinned binding | W01 |
| `M010-W05` | A2A task/message/artifact dedupe, auth-required, loop-depth, idempotency, remote cancellation, signatures and SSRF controls | W04 |
| `M010-W06` | MCP server adapter for intentionally exposed tools/resources/prompts and completion of client callbacks | M006, W01 |
| `M010-W07` | API/OpenAPI/events for agent trees, external tasks, protocol sessions, checkpoints, remote outcomes and experiment assignment/outcome; integrate M003 process/terminal resources | W01-W06, M003-W10, M008 |
| `M010-W08` | Read-only web application using only generated public HTTP/SSE/WebSocket clients | W07 |
| `M010-W09` | Official conformance, adapter round-trip and independent implementation interoperability suites | W02-W08 |
| `M010-W10` | Versioned extension contracts for ACP clients/subagents, A2A peers/skills and intentionally exposed MCP surfaces | W02-W09, M001-W05 |

ACP server, ACP child, A2A, MCP completion and API schema work are parallel after W01. The web app starts after W07 stabilizes.

### 15.3 Exit evidence

- Pinned ACP, A2A and MCP conformance and round-trip suites pass.
- Interoperability succeeds with at least two independent implementations where available.
- Disconnect, duplicate delivery, replay, timeout, auth and cancellation races preserve each protocol's native lifecycle.
- Child writers use separate workspaces, grants never exceed parents, and merges use explicit artifacts.
- Protocol callbacks, elicitation, sampling and messages cannot expand broker authority.
- Remote unknown outcomes are never mislabeled cancelled or retried blindly.
- The web app passes with internal module access disabled and all process, terminal, experiment, agent, checkpoint, approval, cost, prompt, artifact and diff views sourced from authorized public APIs.
- Applicable `KIT-ACP`, `KIT-A2A`, `KIT-MCP`, `KIT-API`, `KIT-OBS`, `KIT-SEC` and `KIT-VERSION` evidence is green.

### 15.4 RFC coverage

Primary: §§23, 27.3, 28. Supporting: §§8, 10, 14-15, 21, 24-27, 31-37.

## 16. KIT-MILESTONE-011: Complete Evaluation and Rollout System

### 16.1 Outcome

Kit can reproducibly decide whether every optimization and policy is correct, safe and valuable, then shadow, canary, promote or roll it back under production-relevant evidence.

### 16.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M011-W01` | Extend Phase 0 immutable manifests with complete task stratification, infrastructure and delayed-outcome metadata | Phase 0-M010 telemetry |
| `M011-W02` | Extend the M003 isolated runner to public/private portfolios and production-equivalent images | M003-W11, W01 |
| `M011-W03` | Extend M004 harness validation with residual nondeterminism, grader mutation and reward-hacking defenses | M004-W12, W02 |
| `M011-W04` | Pinned public benchmark adapters and access-controlled private recent-task corpus | W01-W03 |
| `M011-W05` | Permanent regression, security, mutation/metamorphic, side-effect and act/do-not-act suites | W02-W04 |
| `M011-W06` | Complete RFC §6 reports, ratio-of-totals cost, completion curves, confidence intervals and delayed outcomes | W01-W05 |
| `M011-W07` | Randomized paired/repeated experiment engine with clustering, preregistration, non-inferiority and multiplicity controls | W06 |
| `M011-W08` | Typed replay modes and causal-validity enforcement | W01-W07 |
| `M011-W09` | Shadow execution, sticky canaries, security rollback and review/rework/rollback/defect ingestion | M008, W06-W08 |
| `M011-W10` | Full RFC §§29-30 optimization/ablation portfolio and RFC §35 research disposition reports | M005-M010, W07-W09 |
| `M011-W11` | Requirement evidence dashboard, redacted manual trace audits and release promotion service | Phase 0, W01-W10 |
| `M011-W12` | Complete experiment API/OpenAPI/CLI/SDK resources for manifests, assignments, reports and outcomes | M010-W07, W01-W11 |

Benchmark adapters, private corpus, graders, statistics and production experiment plumbing proceed in parallel around the manifest contract.

### 16.3 Exit evidence

- A trial manifest reconstructs environment and grading inputs; residual provider/model nondeterminism is measured.
- Hidden graders, acceptance rules and gold patches are inaccessible and immutable from the agent sandbox.
- Report validation rejects invalid per-task success ratios and causal claims after trajectories diverge.
- Statistical tests enforce randomization, pairing, clustering, confidence bounds, stopping rules and multiplicity.
- Every required ablation has an accepted report or an explicitly disabled feature/default disposition reflected in the RFC and registry.
- Security failures stop and roll back canaries automatically.
- Delayed human and defect outcomes are present for policies whose promotion depends on them.
- All M001-M010 measured defaults are reevaluated in the complete system, not only component harnesses.
- Experiment resources and delayed outcomes are authorized, versioned and available to the web UI through public APIs only.
- Applicable `KIT-OUTCOME`, `KIT-EVAL`, `KIT-API`, `KIT-CONFIG`, `KIT-OBS`, `KIT-RELEASE` and all experiment-backed requirement evidence is green.

### 16.4 RFC coverage

Primary: §§6, 30, 35. Supporting: §§2-3, 7, 11-22, 28-29, 31-37.

## 17. KIT-MILESTONE-012: Clustered Hostile Multi-Tenant Product

### 17.1 Outcome

Kit operates as a recoverable clustered control plane with remote executors and mutually hostile tenants while preserving the same event, API, ownership, capability and agent semantics as the local daemon.

### 17.2 Work packages

| ID | Deliverable | Depends on |
| --- | --- | --- |
| `M012-W01` | PostgreSQL store matching atomic append, projections, idempotency, commit positions, store-time leases and fencing semantics | M001 |
| `M012-W02` | Object artifact store with upload-before-reference, hash verification, tenant keys, encryption, reachability, retention and GC | M001, W01 |
| `M012-W03` | Cluster event streaming from durable committed-position cursors; notifications only as wakeups | W01 |
| `M012-W04` | Authenticated remote executor protocol with leases, fencing, heartbeats, drain, quiescence, reconciliation and capacity | M003, W01-W03 |
| `M012-W05` | gVisor or per-run microVM hostile backend with COW storage, deny network, egress grants, secret injection and whole-tree kill | M003, W04 |
| `M012-W06` | Tenant isolation across identity, capabilities, providers, caches, indexes, artifacts, telemetry, secrets, egress, quotas and budgets | M006-M010, W01-W05 |
| `M012-W07` | Distributed weighted fairness, admission, quota and spend enforcement | M008, W04-W06 |
| `M012-W08` | Remote mTLS/OIDC ingress, key rotation, revocation and principal mapping | M001, W06 |
| `M012-W09` | PostgreSQL PITR, object restore, legal hold, deletion, backup expiry and disaster recovery automation | W01-W02 |
| `M012-W10` | Expand/contract migrations, rolling upgrades, compatibility manifest and mixed-version window | W01-W08 |
| `M012-W11` | SLOs, dashboards, alerts, capacity plans, incident runbooks, image/SBOM provenance, vulnerability response and one-artifact role packaging | W01-W10 |
| `M012-W12` | Conditional self-hosted inference profile using an existing server plus evaluated batching/cache/quantization/speculation/admission policies | M011, W05-W07 |
| `M012-W13` | Jepsen-style consistency, adversarial tenant, failover, recovery, upgrade and production-topology canary program | W01-W11 |
| `M012-W14` | Versioned store/executor/tenant extension contracts and configuration conformance | W01-W11, M001-W05 |

PostgreSQL/object storage, remote executor/VM, identity/tenant policy and operations are parallel tracks. Multi-tenant admission waits for all tracks and M011 production gates.

### 17.3 Exit evidence

- Partition, delayed commit, node loss, lease expiry, stale writer and cursor tests lose or duplicate no semantic effects.
- Old attempts cannot commit after failover even if their process remains alive.
- DR restores PostgreSQL and artifacts within declared RPO/RTO and verifies hashes, projections, idempotency and deletion state.
- Rolling upgrades pass the declared mixed-version window and migration rollback/forward procedures.
- Adversarial tenants cannot cross filesystem, process, network, credential, cache, artifact, event, trace or catalog boundaries.
- Hostile execution fails closed when VM, egress or secret enforcement is unavailable.
- Load tests prove quota, fairness, interactive SLO and bounded background progress during failures.
- Redaction/retention tests keep secrets and tenant content out of unauthorized prompts, logs, traces, metrics and caches.
- Daemon, control-plane, executor, migration and operational roles ship from one `kit` artifact; local mode requires no internal network service and role separation creates no second domain/API authority.
- The self-hosted profile is evaluated only when selected and does not block clustered/hostile conformance when inapplicable.
- The complete M011 canary and delayed-outcome gates pass on the clustered production topology.
- Applicable `KIT-STORE`, `KIT-EXEC`, `KIT-SEC`, `KIT-RUNTIME`, `KIT-API`, `KIT-CONFIG`, `KIT-OBS`, `KIT-VERSION` and `KIT-RELEASE` evidence is green.

### 17.4 RFC coverage

Primary: §§24-29 and the clustered/hostile end state. Supporting: §§8, 10, 23, 31-37.

## 18. Research Resolution Plan

Research does not block mechanism delivery. It blocks enabling an unproven default. Each question closes with a supported policy or an explicit conservative fallback.

| Question family | Mechanism | Evidence owner | Conservative fallback |
| --- | --- | --- | --- |
| Prompt length and model-specific rules | M002 | M011 | versioned measured core prompt |
| Retrieval evidence and token budgets | M005 | M005/M011 | deterministic bounded progressive discovery |
| LSP/SCIP/graph/semantic value | M005 | M005/M011 | lexical + Tree-sitter + structural + available LSP |
| Compact schema notation | M006 | M006/M011 | provider-valid JSON Schema |
| Grammar-constrained patch output | M004/M006 | M011 | ordinary validated calls |
| Edit format routing | M004 | M011 | simplified context patch with exact conflicts |
| Fast-check prediction | M004/M005 | M011 | syntax/format/diagnostic and explicit policy floors |
| Expected-value stopping | M008 | M011 | fixed budgets and repeated-failure thresholds |
| Direct/parallel/host-macro/Lua/Runlet routing | M007/M008 | M007/M011 | direct calls or explicit backend |
| Runlet familiarity cost | M007 | M011 | direct/Lua economic default |
| TOON classifier | M007 | M007/M011 | canonical compact JSON |
| Checkpoint signaling | M009 | M009/M011 | explicit `yield` at safe boundaries |
| Continuation quality scoring | M009 | M011 | deterministic reconciliation and fallback |
| Shared prefix warming | M008 | M011 | instrumentation on, warming off |
| Subagent decomposition | M010 | M011 | explicit bounded child creation |
| A2A vs ACP vs direct tools | M010 | M011 | explicit protocol selection |
| Confidence calibration | M008 | M011 | conservative risk floors and approval |
| Replay predictiveness | M011 | M011 | fresh trials for quality claims |

If evidence rejects an RFC assumption, update the RFC and tombstone or supersede the affected requirements before claiming completion. Do not preserve contradictory prose while disabling the implementation.

## 19. Release Stages

| Stage | Minimum gate | Promotion condition |
| --- | --- | --- |
| Component build | relevant milestone | all milestone requirements and evidence green |
| Trusted-local dogfood | M004 | restart, edit recovery, conflict, cancellation and restricted execution pass |
| Intelligence preview | M005 | core localization beats lexical baseline without freshness regression |
| Capability/composition preview | M007 | broker authority, nested budgets, schema pinning and canonical fallback pass |
| Optimized local beta | M008 + M009 | routing and compaction pass non-inferiority with deterministic fallbacks |
| Protocol beta | M010 | protocol conformance, remote lifecycle and child isolation pass |
| Controlled production canary | M011 | sticky assignment, external grading, delayed outcomes and rollback pass |
| Single-tenant production | M011 | no safety regression and optimization promotion rules pass |
| Clustered hostile multi-tenant | M012 | isolation, failover, fairness, PITR and adversarial suites pass |
| Initial complete product | final join | completion contract in §2 passes against one release candidate |

Any security or unauthorized-effect failure blocks promotion and triggers rollback. An optimization failure disables that optimization but need not remove a correct underlying mechanism.

## 20. RFC Section Coverage

| RFC section | Owner and completion path |
| --- | --- |
| §1 Summary | M001 establishes topology; M001-M012 realize the complete promise |
| §2 Motivation | M008 implements efficiency policy; M011 proves trajectory improvements |
| §3 Goals | Cross-cutting M001-M012; M011 owns outcome evidence |
| §4 Non-Goals | Phase 0 architecture assertions; audited at every milestone |
| §5 Normative Language | Phase 0 registry and CI gate |
| §6 Success Definition | M002 instrumentation, M008 policy, M011 complete evaluation |
| §7 Product Principles | M001 authority, M002/M004/M006-M009 behavior, M011 validation |
| §8 System Architecture | M001 module boundaries; M002-M012 complete modules |
| §9 Agentkit | M002 base integration; M006/M007/M009/M010 extension points |
| §10 Durable Domain | M001 core; M002/M003/M009/M010 entities; M012 clustered store |
| §11 Prompt System | M002 implementation; M011 deletion/model ablations |
| §12 Context/Caching | M002 core, M008 policy, M009 compaction, M011 evidence |
| §13 Repository Intelligence | M004 lexical core; M005 complete adapters and policies |
| §14 Tool Surface | M004 core, M006 schemas/tools, M007 compose, M009 yield, M010 agent |
| §15 Dynamic Tools | M006 implementation; M010 protocol catalog sources; M011 learning evidence |
| §16 Composition | M007 implementation; M011 confirmatory evaluation |
| §17 TOON | M007 implementation; M011 default evidence |
| §18 Editing | M003 staging boundary, M004 transaction, M005 semantic adapters, M011 evaluation |
| §19 Verification | M004 fast ladder, M005 affected checks, M008 loop policy, M011 evaluation |
| §20 Routing | M008 implementation; M011 promotion evidence |
| §21 Parallelism | M003 isolation, M008 runtime, M010 children, M012 distributed fairness |
| §22 Compaction | M009 implementation; M011 state-transfer confirmation |
| §23 Protocols | M006 MCP broker; M010 ACP/A2A/MCP server and lifecycle |
| §24 Security | M001 identity, M003 isolation, M006 broker, M007 nesting, M010 remote, M012 tenant |
| §25 Isolation | M003 local/restricted, M010 child cases, M012 hostile tier |
| §26 Cancellation/Scheduling | M001 lifecycle, M002 loop, M003 reaping, M008 queues, M010 remote, M012 failover |
| §27 Public API | M001 core/parity; every milestone extends it; M010 clients; M012 remote operations |
| §28 Observability | M001/M002 instrumentation, M010 UI, M011 experiments, M012 operations |
| §29 Optimizations | M002/M004-M009 mechanisms, M011 acceptance, M012 self-hosted profile |
| §30 Evaluation | continuous from Phase 0; M011 complete program |
| §31 Configuration | M001 layering; each adapter milestone extends; M012 tenant/executor layers |
| §32 Versioning | Phase 0 framework; milestone-specific pins; M012 rolling compatibility |
| §33 Risks | Phase 0 registration; owning milestones implement; M011/M012 validate |
| §34 Delivery | M001-M012 and final joined gate |
| §35 Research | mechanism owners plus M011 experimental governance |
| §36 Decisions | Phase 0 assertions; M001-M012 realization; final architecture audit |
| §37 References | Phase 0 build pins; adapters/harnesses update compatibility manifests |

## 21. Final Joined Verification

After M012 implementation freezes, build one release candidate from a clean checkout and run this sequence without substituting historical artifacts:

1. Verify build provenance, dependency pins, generated schemas and requirement registry.
2. Install local daemon from scratch and migrate every supported prior persistence version.
3. Run domain, store, API, CLI, auth, retention, deletion, backup and restore conformance.
4. Run agentkit intent/outcome, prompt, context, usage, interruption and safe-resume suites.
5. Run restricted and hostile executor, workspace, process, terminal, secret and cancellation adversarial suites.
6. Dogfood a real Kit change through API/CLI with discovery, edit, verification, events, cost and recovery.
7. Run syntax, LSP, history, affected-check and optional-adapter-contract suites, plus SCIP/graph/sparse/dense evaluations for every selected adapter.
8. Run capability, schema, MCP, composition, TOON and nested-policy conformance.
9. Run router, scheduler, batching, speculation, fairness, memoization, predicted-output, build-cache, batch-lane, evidence-ranking, review and fallback evaluations.
10. Run compaction state-transfer, crash, rejection and resume suites.
11. Run ACP/A2A/MCP interoperability, child isolation, remote cancellation and UI public-API-only suites.
12. Run the complete public/private/adversarial benchmark portfolio and all required ablations.
13. Run PostgreSQL/object-store consistency, failover, PITR, rolling upgrade and deletion drills.
14. Run cross-tenant hostile red-team and distributed load/fairness tests.
15. Run a production-topology shadow and sticky canary long enough to collect required delayed outcomes.
16. Verify layered configuration and every extension contract, then generate the final traceability report and manually audit the limited structural/documentation evidence.

The release is RFC-complete only when the joined run reports:

```text
normative evidence coverage:        100%
architectural promise resolution:   100%
RFC section coverage:               37/37
milestone gates passing:            12/12
open release blockers:              0
unresolved security findings:       0
unresolved research defaults:       0
```

## 22. First Execution Queue

Begin with these tasks in order:

1. Extract and atomize RFC commitments into the requirement registry.
2. Review the registry for compound clauses, applicability and missing architectural promises.
3. Add requirement lint and the empty evidence report to CI.
4. Pin Rust, agentkit, Runlet and protocol/schema versions in the build manifest.
5. Create the one-binary Cargo workspace and internal module skeleton.
6. Define domain IDs, lifecycle commands/events and ownership invariants for M001-W01.
7. Write store contract tests before selecting SQLite implementation details.
8. Implement the SQLite event/projection/idempotency transaction against those tests.
9. Implement bounded scheduling, layered configuration, the security substrate, auth contract and capability kernel.
10. Add policy-aware artifacts, deletion, backup/restore and fault injection before exposing the first public mutation.
11. Build the command/query service, authenticated API/SSE and thin CLI over the same handlers.

Do not begin the agent loop, tool implementations, or repository mutation until Phase 0 and the corresponding durable control-plane contracts are passing.
