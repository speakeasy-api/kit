# Fault Matrix

Unit `1.03`. Maps each of the 10 boundaries in `docs/decisions/threat-model.md`
onto the 7 reusable fixture families registered for unit `1.11`
(`IMPLEMENTATION_PLAN.md:191`): `providers`, `repos`, `protocol_sim`, `clock`,
`crashpoints`, `storefault`, `sandbox_probe`. Boundary numbers and titles are
identical to `threat-model.md` so the two documents stay in lockstep; checked
by `scripts/lint_threat_model.sh`.

Each row is one fault injection: the fixture family that produces it, the
concrete injection, the invariant from `RFC.md` that must hold across it, and
the evidence a harness records to prove it held (or caught the violation).
Every intent fixes all external inputs and requires byte-equivalent replay;
wall-clock time, live networks, and provider behavior are never test oracles.

The cross-cutting sub-boundaries below map into the canonical inventory rather
than adding new plan boundaries.

Evidence is statused per row. `Implemented` names an existing Cargo test target
and exact test function (or a script and concrete check). `Planned` names the
accountable milestone owner and acceptance gate; planned rows are not verified
evidence. The linter resolves implemented references and keeps the two statuses
distinct in its summary.

### Sub-boundary: CI and release evidence authority (maps to Boundary 9)

**Boundary class:** authority + persistence.

**Deterministic fixture intent:** `protocol_sim` and `storefault` replay fixed
candidate digests, lane identities, evidence states, and artifact availability;
each replay must make the same promotion decision and name the same rejected row.

**Evidence owner:** `M011-W11` owns release evidence authority and promotion.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| protocol_sim | Replay a passing evidence envelope under a different candidate digest | Evidence is authoritative only for its bound candidate revision (`RFC.md` §34) | Planned: owner `M011-W11`; gate `candidate-digest binding acceptance`. |
| storefault | Withhold a required artifact after its evidence row is announced | Missing or pending evidence blocks release promotion (`RFC.md` §34) | Implemented: `tests/integration/main.rs::release_candidate_rejects_pending_evidence` observes a nonzero release-candidate result for pending evidence. |

### Sub-boundary: Hidden-grader custody (maps to Boundary 4)

**Boundary class:** authority.

**Deterministic fixture intent:** `repos` and `sandbox_probe` replay fixed
workspace paths, symlink targets, grader mount identities, and backend status;
each replay must produce the same denied paths and fail-closed startup decision.

**Evidence owner:** `M003-W11` owns hidden-grader and gold-material custody.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| repos | Add a workspace symlink targeting evaluator-owned hidden material | Trial code cannot read hidden graders, gold patches, or acceptance rules (`RFC.md` §29) | Implemented: `tests/adversarial/main.rs::malicious_repository_paths_hooks_and_custody_are_denied` records symlink denial and no accepted escape path. |
| repos | Add traversal and executable-hook entries that attempt to replace grader material | Repository content cannot mutate evaluator custody (`RFC.md` §29) | Implemented: `tests/adversarial/main.rs::malicious_repository_paths_hooks_and_custody_are_denied` records traversal and hook denials. |

### Sub-boundary: Configuration and extension loading (maps to Boundary 6)

**Boundary class:** authority.

**Deterministic fixture intent:** `repos` and `protocol_sim` replay fixed YAML
records, aliases, extension identities, and discovery/invocation schema digests;
each replay must produce the same duplicate-record and schema-drift rejection.

**Evidence owner:** `M001-W05` owns configuration loading and `M006-W11` owns
extension registry and compatibility enforcement.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| repos | Load two YAML records with one identity through an alias | Ambiguous configuration is rejected rather than resolved by record order (`RFC.md` §31) | Implemented: `tests/adversarial/main.rs::duplicate_yaml_records_are_rejected` invokes the requirement linter and observes duplicate-requirement-id. |
| protocol_sim | Change an extension schema digest between discovery and invocation | An extension cannot gain authority after its schema binding is approved (`RFC.md` §24, §31) | Implemented: `tests/adversarial/main.rs::extension_schema_drift_is_refused` observes schema-drift refusal from the protocol simulator. |

## Boundary 1: Local and remote API ingress

**Boundary class:** authority.

**Deterministic fixture intent:** `protocol_sim` replays a fixed principal,
header set, idempotency key, and pair of request digests in a fixed order; each
replay must produce the same response codes and command-dispatch count.

**Evidence owner:** `M001-W07` for transport rejection and `M001-W11` for
idempotent command dispatch.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| protocol_sim | Forge/strip `Origin`/`Host` on a loopback HTTP request | Transport authentication rejects the request before command dispatch (`RFC.md` §27.1) | Planned: owner `M001-W07`; gate `transport-authentication acceptance`. |
| protocol_sim | Replay one `Idempotency-Key` with a divergent request digest | Reuse with different input returns conflict; it never silently applies a second mutation (`RFC.md` §27.1) | Planned: owner `M001-W11`; gate `idempotent-command acceptance`. |

## Boundary 2: State root, SQLite and artifacts

**Boundary class:** persistence.

**Deterministic fixture intent:** `storefault` replays fixed crashpoint IDs,
event bytes, artifact bytes, and expected stream versions against a fresh
state root; each replay must produce the same recovered log and hash result.

**Evidence owner:** `M001-W02` for commit/replay and `M001-W09` for
hash-before-reference.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| storefault | Kill the daemon between WAL commit and projection update | Startup orphan reconciliation restores consistent projections; `(stream, sequence)` and `commit_position` stay unique (`RFC.md` §10.4) | Planned: owner `M001-W02`; gate `state-root replay acceptance`. |
| storefault | Corrupt/withhold artifact bytes after upload-confirm, before hash verification | Artifacts are hash-verified before any event references them (`RFC.md` §10.4) | Planned: owner `M001-W09`; gate `hash-before-reference acceptance`. |

## Boundary 3: Workspace mutation and hostile repositories

**Boundary class:** authority + persistence.

**Deterministic fixture intent:** `repos` replays a fixed repository tree,
hook payload, base revision, and writer schedule; each replay must produce the
same denied path set, writer decision, and final diff digest.

**Evidence owner:** `M003-W02` for writer/revision behavior and `M003-W08` for
hostile repository execution.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| repos | Malicious-repository fixture with a hook that writes outside the workspace root | Hooks/submodules disabled or sandboxed by default; workspace-root boundary holds (`RFC.md` §25.3) | Planned: owner `M003-W08`; gate `hostile-repository execution acceptance`. |
| repos | Assign two run attempts to one workspace snapshot | A workspace has at most one mutable writer unless an explicit merge is active (`RFC.md` §10.3) | Planned: owner `M003-W02`; gate `workspace-writer arbitration acceptance`. |

## Boundary 4: Local, container and VM execution

**Boundary class:** authority.

**Deterministic fixture intent:** `sandbox_probe` and `crashpoints` replay a
fixed executor profile, probe list, unavailable-backend flag, and virtual
deadline; each replay must produce the same denied probes and typed refusal.

**Evidence owner:** `M003-W01` for fail-closed profiles,
`M003-W03`/`M003-W04` for isolation, and `M012-W05` for hostile execution.

The deterministic fixtures below are local source/conformance evidence. Actual
filesystem, network, resource, process-tree, trial, and helper enforcement is a
separate external lane bound to `EXT-01`, `EXT-04`, `EXT-19`, `EXT-20`, and
`EXT-22`; a fixture pass does not pass any platform or architecture cell.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| sandbox_probe | Probe reachability of Docker socket, SSH agent, cloud metadata IP from inside the sandbox | No Docker socket, SSH agent, host daemon socket, or cloud metadata reachable (`RFC.md` §25.2) | Planned: owner `M003-W03`; gate `restricted-isolation acceptance`. |
| crashpoints | Simulate the hostile tier's isolation backend (gVisor/microVM) unavailable at executor start | Fail-closed when required isolation is unavailable (`RFC.md` §25.2) | Planned: owner `M003-W01`; gate `fail-closed executor-profile acceptance`. |

## Boundary 5: Model/provider calls and prompt injection

**Boundary class:** authority.

**Deterministic fixture intent:** `providers` replays fixed model chunks,
injected repository bytes, grant tuples, and canary secrets; each replay must
produce the same broker denial and zero persisted canary occurrences.

**Evidence owner:** `M001-W08` for kernel denials and `M006-W03` for broker
prompt-injection behavior.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| providers | Fake provider echoes repository-content instruction as an out-of-grant tool call | Repository text/tool descriptions/web content are data, not authority; broker denies out-of-grant effects (`RFC.md` §24) | Planned: owner `M006-W03`; gate `prompt-injection broker acceptance`. |
| providers | Fake provider attempts to place a resolved secret value into a proposed prompt/event payload | Secret values must not appear in prompts or events (`RFC.md` §24) | Planned: owner `M001-W06`; gate `secret-redaction persistence acceptance`. |

## Boundary 6: Native, MCP and composed capabilities

**Boundary class:** authority.

**Deterministic fixture intent:** `protocol_sim` replays fixed discovery and
invocation schemas, parent grants, delegation depth, and nested call order;
each replay must produce the same schema refusal and authorization trace.

**Evidence owner:** `M006-W05` for immutable binding and `M007-W01` for nested
broker invocation.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| protocol_sim | Change an MCP tool's schema digest between `tools/list` and `tools/call` | Broker enforces pinned schema digests for nested and direct calls (`RFC.md` §23.5) | Planned: owner `M006-W05`; gate `immutable capability-binding acceptance`. |
| protocol_sim | Composed-program fixture issues a nested call exceeding parent delegation depth/grant | One broker is sole authority for direct and nested calls (`RFC.md` §24) | Planned: owner `M007-W01`; gate `nested-call authorization acceptance`. |

## Boundary 7: ACP children and A2A peers

**Boundary class:** authority + persistence.

**Deterministic fixture intent:** `protocol_sim` and `crashpoints` replay fixed
remote identities, sequences, digests, delegation paths, and child crashpoint;
each replay must produce the same dedupe and durable lifecycle states.

**Evidence owner:** `M010-W03` for ACP supervision and `M010-W05` for A2A
dedupe, loop, authentication, and cancellation behavior.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| protocol_sim | Replay a duplicate A2A task/message sharing remote identity and digest | Messages/artifacts deduplicated by remote identity and sequence/digest (`RFC.md` §23.4) | Planned: owner `M010-W05`; gate `A2A replay-deduplication acceptance`. |
| protocol_sim | Construct an A2A delegation chain exceeding the depth/path token limit | Delegation carries a depth/path token to reject loops (`RFC.md` §23.4) | Planned: owner `M010-W05`; gate `A2A delegation-loop acceptance`. |
| crashpoints | Kill a local ACP child process mid-tool-call | Cancellation terminal only after acknowledgment/confirmed quiescence (`RFC.md` §23.3, §26) | Planned: owner `M010-W03`; gate `ACP cancellation-lifecycle acceptance`. |

## Boundary 8: Secrets, URLs, redirects and egress

**Boundary class:** authority.

**Deterministic fixture intent:** `protocol_sim` and `sandbox_probe` replay a
fixed redirect graph, DNS answer sequence, egress grant, and canary secret;
each replay must deny the same hop with zero canary disclosure.

**Evidence owner:** `M001-W06` for secrets and base egress policy and
`M006-W08` for MCP URL hardening.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| protocol_sim | Serve a redirect chain ending on a private-range/metadata address | Every discovered URL is defended against SSRF and redirects (`RFC.md` §24) | Planned: owner `M006-W08`; gate `redirect SSRF acceptance`. |
| sandbox_probe | Flip a hostname's resolved address between validation and connect (DNS rebinding) | Egress defended against DNS rebinding (`RFC.md` §24) | Planned: owner `M006-W08`; gate `DNS-rebinding acceptance`. |

## Boundary 9: Telemetry, retention, backups and deletion

**Boundary class:** persistence.

**Deterministic fixture intent:** `storefault` replays fixed backup bytes,
retention policy, reachability graph, and delete/backup interleaving; each
replay must produce the same health and earliest-deletion decisions.

**Evidence owner:** `M001-W13` for restore, `M001-W14` for deletion, and
`M001-W15` for telemetry retention and leakage.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| storefault | Restore a backup snapshot with truncated/corrupted bytes injected | Backups are automatically verified restorable (`RFC.md` §10.4) | Planned: owner `M001-W13`; gate `backup-restore verification acceptance`. |
| storefault | Race a delete job against a concurrent backup capturing the same content | Delete never silently removes still-referenced/backed-up content (`RFC.md` §27.1) | Planned: owner `M001-W14`; gate `retention-aware deletion acceptance`. |

## Boundary 10: Clustered control plane and executors

**Boundary class:** authority + persistence.

**Deterministic fixture intent:** `clock`, `storefault`, and `sandbox_probe`
replay fixed virtual-clock ticks, lease/fence values, partition schedule,
tenant load, and scheduler seed; each replay must produce the same accepted
commit prefix and zero stale or cross-tenant effects.

**Evidence owner:** `M012-W01` for clustered persistence, `M012-W04` for
executor leases, and `M012-W06`/`M012-W07` for isolation and fairness.

| Fixture Family | Fault Injection | Expected Invariant | Evidence |
| --- | --- | --- | --- |
| clock | Advance one node's lease clock past expiry while a partition hides it from the other node | Stale attempt cannot commit after losing its lease; fencing tokens never derive from timestamps (`RFC.md` §10.4, §26) | Planned: owner `M012-W04`; gate `cluster lease-fencing acceptance`. |
| storefault | Partition the clustered store mid commit-serialization | A later-visible transaction never lets a reader skip an earlier one (`RFC.md` §10.4) | Planned: owner `M012-W01`; gate `committed-prefix serialization acceptance`. |
| sandbox_probe | Saturate one tenant's quota in a hostile multi-tenant fixture | Weighted fairness scheduling; interactive runs keep latency priority (`RFC.md` §26, §25.1) | Planned: owner `M012-W07`; gate `multi-tenant fairness acceptance`. |
