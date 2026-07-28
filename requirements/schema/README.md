# Registry record schema

`record.schema.json` is the JSON Schema (2020-12 dialect) for one row of the
Kit RFC 0001 requirement registry (Phase 0 §4.1,
`IMPLEMENTATION_PLAN.md:83-134`, plan.md row `1.01`). Every record in
`requirements/registry.d/KIT-<AREA>.yaml` and in the three cross-cutting
shards `_promises.yaml`, `_decisions.yaml`, `_risks.yaml` validates against
this schema.

## Fields (39, all required)

The `required` array is exactly the 39 identifiers of
`IMPLEMENTATION_PLAN.md:124-133`, no more, no fewer. Fields whose value may
legitimately be absent at record-creation time (`supersedes`,
`tombstone_reason`, `artifact_digest`, `environment_digest`, `versions`,
`decision_record`, `deviation_record`) are still required *properties*; they
are typed `["<type>", "null"]` and carry `null` until populated, rather than
being omitted. This keeps every record shape-complete (no unregistered
required-field CI failures, per `IMPLEMENTATION_PLAN.md:139-140`) while still
letting evidence and governance fields fill in over a record's lifecycle.

| Field | Meaning |
| --- | --- |
| `id` | Stable `KIT-<AREA>-NNN` identifier (never reused). |
| `record_class` | `requirement` \| `promise` \| `decision` \| `risk`. |
| `modality` | RFC 2119 keyword (`MUST`, `MUST NOT`, `SHOULD`, `SHOULD NOT`, `MAY`) or `declarative` for non-normative promises/decisions/risks. |
| `title` | Short human label. |
| `atomic_text` | The single atomic statement, split from compound prose. |
| `source_section` | RFC.md section, `§<n>[.<n>]` optionally followed by its title. |
| `source_anchor` | Exact `RFC.md:<line>` or `RFC.md:<lo>-<hi>` the text was lifted from. |
| `source_quote` | Verbatim quoted RFC text. |
| `source_fingerprint` | SHA-256 hex of the stripped text covered by `source_anchor` (the complete joined span for a range), for drift detection. |
| `introduced_revision` | Resolvable RFC.md git commit (short or full SHA-1) where the cited requirement text first appeared, not the registry-row creation revision. |
| `status` | `proposed` \| `active` \| `implemented` \| `mitigated` \| `not_selected` \| `resolved_by_amendment` \| `tombstoned`. |
| `supersedes` | id of the record this one replaces (tombstoned records only); else `null`. |
| `tombstone_reason` | Why a `tombstoned` record was retired; `null` otherwise. |
| `area` | One of the 29 normalized `KIT-<AREA>` prefixes. |
| `applicability` | `mandatory` \| `selected` \| `not_selected` \| `pending_voi` \| `not_applicable`. |
| `interpretation` | Operational reading of `atomic_text`. |
| `acceptance_criteria` | ≥1 machine-decidable criterion (banned vague words rejected). |
| `primary_milestone` | Owning milestone `M001`-`M012`, or `PHASE0`. |
| `contributing_milestones` | Additional milestones extending/depending on this record. |
| `dependencies` | ids of other records this one depends on. |
| `owner` | Accountable role/team. |
| `criticality` | `blocking` \| `high` \| `medium` \| `low`. |
| `platforms` | Applicable platform cells (`macos-arm64`, `linux-x86_64`, `linux-aarch64`, `windows`, `all`). |
| `deployment_tiers` | Applicable deployment modes (`local`, `restricted`, `clustered`, `hostile`). |
| `release_gates` | Milestone gate(s) fed (`G00`-`G12`, optionally `-CLUSTERED`). |
| `implementation_links` | Paths implementing this record; may be empty pre-implementation. |
| `public_contract_links` | Public contract surface(s) bound by this record; may be empty. |
| `telemetry_links` | Telemetry needed to reproduce the acceptance result; may be empty. |
| `evidence_type` | `conformance` \| `evaluation` \| `operational_assertion` \| `manual_review`. |
| `evidence_id` | Canonical `EV-<parent WP>-<C\|E\|O\|M>-<NNN>` form. |
| `evidence_job` | One of the 12 fixed CI lanes (unit `1.13`). |
| `expected_result` | Exact expected result of `evidence_job` (banned vague words rejected). |
| `artifact_digest` | SHA-256 hex of the evidence build artifact; `null` until the job runs. |
| `environment_digest` | SHA-256 hex of the evidence execution environment; `null` until the job runs. |
| `versions` | Pinned tool/dependency versions recorded by the job; `null` until the job runs. |
| `latest_result` | `pending` \| `pass` \| `fail` \| `outcome_unknown`. |
| `revalidation_rule` | Condition under which existing evidence goes stale. |
| `decision_record` | Path to a governing `docs/decisions/DR-NNNN-<slug>.md`; `null` if none. |
| `deviation_record` | Pointer to a documented SHOULD/SHOULD-NOT exception; `null` if none. |

## Enums grounded in the plan

- **`area`** (29 values): the normalized prefixes of `IMPLEMENTATION_PLAN.md:89-117`
  with the trailing `-` stripped (`KIT-GOV-` → `KIT-GOV`, etc.), byte-equal to
  the `requirements/registry.d/KIT-<AREA>.yaml` shard basenames and to the
  `--areas` tokens `1.04`-`1.07` accept. Registry shard names and `--areas`
  arguments must already use this normalized form; a trailing `-` is rejected.
- **`evidence_type`** / the `C`/`E`/`O`/`M` codes: the exact allowed set of
  `IMPLEMENTATION_PLAN.md:19` and the one-and-only type↔code map of plan.md §5.
- **`evidence_job`**: the fixed 12-lane set unit `1.13` creates (`fmt`, `lint`,
  `unit`, `integration`, `req-lint`, `schema-compat`, `fault`, `adversarial`,
  `reproducible-build`, `licenses`, `vuln-scan`, `evidence-report`); no other
  lane name is legal.
- **`modality`**: the only RFC 2119 keywords that actually occur in `RFC.md`
  (`MUST`, `MUST NOT`, `SHOULD`, `SHOULD NOT`, `MAY`), plus `declarative` for
  the non-normative classes Phase 0 §4.2 requires registering anyway.
- **`deployment_tiers`**: `local`, `restricted`, `clustered`, `hostile` per the
  Objective section's four deployment modes.
- **`platforms`**: the cells of the §9.2 OS/platform variants matrix.
- **`status`**: `implemented` and `resolved_by_amendment` are the literal
  terminal values `IMPLEMENTATION_PLAN.md` T2 checks for architectural-promise
  resolution; `not_selected` is the literal optional-mechanism disposition
  used throughout §6 (`1.15`, `6.10`, `14.11`, ...); `tombstoned` is the
  literal retirement state of `IMPLEMENTATION_PLAN.md:89,126,142`; `proposed`/
  `active` are the pre-terminal "live requirement" states implied by
  `IMPLEMENTATION_PLAN.md:140`'s contrast with tombstoning.

## Cross-record consistency the schema does *not* enforce

JSON Schema validates one instance document at a time; it cannot check a
record against the rest of the registry. The following are enforced by
`scripts/req_lint.py`, not by this schema, and are noted here so no one reads
the schema as the sole gate:

- `id`'s `KIT-<AREA>` prefix must equal the record's own `area` field.
- `id` must be globally unique across the registry (no duplicate/reused ids).
- Normative source anchors must still cover unchanged RFC text.
- Tests and evaluations may cite only registered ids.
- Live records need ownership, milestone, acceptance criteria, and an evidence
  plan; tombstones need a replacement or decision record; release evidence
  must be present, current, and passing.

## In-schema cross-field rules that *are* enforced

- `status: tombstoned` requires a non-null `tombstone_reason` and at least one
  of `supersedes` or `decision_record`; any other `status` forces both
  `tombstone_reason` and `supersedes` back to `null`.
- `record_class: promise` forces `modality: declarative`; `record_class:
  requirement` forces `modality` to be a real RFC 2119 keyword (never
  `declarative`), because every normative statement carries an actual keyword
  in `RFC.md`.
- `latest_result: pass` or `fail` requires non-null `artifact_digest`,
  `environment_digest`, and a non-empty `versions` object — an evidence run
  that actually executed always has recorded provenance.
- `acceptance_criteria` items and `expected_result` reject the banned vague
  words listed in plan.md §5 (`robust`, `complete`, `appropriate`, `proper`,
  `sufficient`, `good`, `clean`, `reasonable`, `as needed`); each criterion
  must instead name an exact command/check plus an exact expected outcome.
- `additionalProperties: false` — a record with a field not in the 39 is
  rejected, matching the "0 extra" half of the required-set acceptance
  criterion at the record level as well as the schema level.

## Validation

```sh
check-jsonschema --check-metaschema requirements/schema/record.schema.json
check-jsonschema --schemafile requirements/schema/record.schema.json <record-file.json>
```

`scripts/req_lint.py` uses the installed PyYAML and `jsonschema` facilities to
load shards and apply this schema. It does not install dependencies. Use
`check-jsonschema` (or an equivalent 2020-12 validator) for the standalone
metaschema check above.
