# DR-0002: Registry Sharding

Status: accepted.

## Context

`IMPLEMENTATION_PLAN.md:165` requires generated
`requirements/registry.yaml`, `requirements/evidence.yaml`, and
`requirements/tombstones.yaml`. Parallel extraction into one registry file
would create a shared-writer bottleneck and unstable ID allocation.

## Decision

Authoritative source records are partitioned into one
`requirements/registry.d/KIT-<AREA>.yaml` shard for each of the 29 normalized
areas plus `_promises.yaml`, `_decisions.yaml`, and `_risks.yaml`. One unit owns
each shard. The integrator alone generates `requirements/registry.yaml` from
all shards while holding `requirements/merge-lock.yaml`.

The generated aggregate remains the plan-required external artifact. Shards
are an authoring boundary, not a second registry or a change to record
semantics.

## Invariants

- Every record ID is globally unique across all shards.
- An area shard contains only records whose `area` equals its basename.
- Cross-cutting shards still use IDs from the 29 registered areas.
- Aggregate count equals the sum of shard counts.
- Aggregate generation is deterministic and integrator-owned.

## Verification

`scripts/req_lint.py --aggregate` exits 0, aggregate count equals the shard
sum, and duplicate ID count is 0. Reopen if deterministic generation or
single-owner integration cannot be maintained.
