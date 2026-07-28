# DR-0005: Optional Mechanism Applicability

Status: accepted policy; all mechanism outcomes pending evidence.

## Decision

`requirements/policy/optional.yaml` is the authoritative applicability
register for all 35 optional registry mechanisms, including those named by RFC
§13 layers 5-7, §16 compose backends, §17 TOON presentation, and §29.5
self-hosted inference. Each
mechanism has exactly one `selected`, `not_selected`, or `pending_voi`
disposition, one gate, one experiment ID, one predeclared selection rule, and
one conservative fallback.

No `selected` or `not_selected` outcome is recorded in Phase 0 because the
experiments have not run. Every row therefore begins `pending_voi` with null
evidence. This records an evidence plan, not a result.

## Selection Rules

Repository adapters require incremental localization value under the declared
token budget while downstream correctness remains within its preregistered
non-inferiority margin. Compose and encoding mechanisms require task-level
correctness non-inferiority plus a confidence interval excluding zero for the
declared cost, token, latency, or error estimand. Self-hosted mechanisms also
must satisfy isolation, starvation, and p99 conditions where those risks
apply.

An accepted report changes a row to `selected` and links its evidence. A
rejected report changes it to `not_selected`, links its evidence, and leaves
the row's fallback active. No `pending_voi` row may survive its owning gate.
Missing evidence is not evidence for either disposition.

## Named Mechanisms

The register contains four repository adapters, two compose backends, TOON,
the self-hosted profile, each of its nine named optimization mechanisms, and
18 additional optional requirement mechanisms. Sparse and dense retrieval and
grouped-query and quantized KV are separate rows because they can produce
different evidence and dispositions. All 35 outcomes remain `pending_voi`.

## Verification

Policy validation must report 35 mechanism rows, 35 distinct IDs, 35 distinct
experiment IDs, zero dispositions outside the allowed set, and zero
`pending_voi` rows after their owning gate.
