# Preregistration Schemas

`v1/components.schema.json` is the authoritative component set reused by the plan, registration, and
report wrapper schemas. It fixes roles, units, bounded text/numbers, non-zero digests, authority-era UTC
timestamps, and metric-specific roles, units, directions, margins, report interval methods, and trial
status shapes. Runtime validation uses the same components before applying the explicitly runtime-only
relations: ordering, cross-object metric equality, pair equality, margin ordering, roster cardinality,
and canonical commitment equality.

Task-set and dataset commitments are derived from the canonical roster. The experiment-design digest is
derived from that roster, the fixed plan, and the invariant harness pin; supplied values must match. Each
roster entry commits its exact model, model-settings, run-config, and provider-capability digests. Each
pair has exactly equal task ID, dataset member, task-manifest digest, and seed across arms.
Plans contain no timestamps and specify no retry or missing-pair choice: one attempt is analyzed and any
missing or incomplete pair rejects completion.
The required `policies.error_imputation` object preregisters positive conservative maximum cost and
latency values for terminal outcomes whose continuous evidence is unavailable.
