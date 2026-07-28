# DR-0006: Empty Area Not-Applicable Register

Status: accepted.

## Decision

An area shard may contain zero records only when
`requirements/policy/area-na.yaml` contains that exact normalized area prefix
and cites RFC or implementation-plan text proving that the area is outside the
product scope. Convenience, delayed work, missing extraction, and optional
implementation are not not-applicable reasons.

The current register is empty. No area is declared not applicable: the
29 prefixes in `IMPLEMENTATION_PLAN.md:89-117` all describe the initial
complete product, and the extraction and integration units expect every area
to have at least one record. Optional mechanisms are handled by
`requirements/policy/optional.yaml`, not by making their entire area absent.

## Verification

The integrator rejects every empty `KIT-<AREA>.yaml` shard whose prefix is not
listed with a cited reason. It also rejects unknown prefixes and an area-NA
entry whose shard contains a live record. Reopen only when authoritative scope
text makes an entire area inapplicable.
