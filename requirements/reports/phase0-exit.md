# Phase 0 Exit Report

- Gate: `G00`
- Run date: `2026-07-28`
- Result: **PASS (local G00, 13/13 implemented records)**
- Candidate identity: `worktree:ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Source-tree SHA-256: `ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Local attestation-set SHA-256: `59911cbd6ffb954a5bcab88c5fa54c4a9ec007bc6c75acb8dc7322f1ba5281fd`
- Test jobs: **2/2 selected at least one passing test**
- Non-test proof jobs: **11/11 produced authoritative output**
- Release result: **EXPECTED FAIL**

The candidate identity is intentionally the current worktree digest, not the docs-only
base commit `0621d6f5a476b9714d606a1ff305592aae301103`. The digest input is the sorted manifest
`<sha256><two spaces><mode><two spaces><path>` over every source-tree file. It
excludes `.git/`, `.superworkflow/`, `.evidence-tmp/`, `.tools/`, every `target/`
and `__pycache__/`, `*.pyc`, `.DS_Store`, `requirements/attestations/**`, generated
projections/milestone exit reports, and retained M004 report artifacts. Registry shards are
hashed canonically with only run-derived artifact/environment/version fields nulled, avoiding
an evidence self-reference while retaining all requirement and command semantics. Source-controlled attestations
are local-only and rejected for release.
The legitimate non-test jobs are compile-only `EV-1.04-C-004`; governance validators
`EV-1.04-C-001`, `EV-1.04-C-002`, `EV-1.04-C-003`, `EV-1.04-C-800`,
`EV-1.04-C-803`, `EV-1.04-C-804`; and pin validators `EV-1.09-C-001` through
`EV-1.09-C-004`. The compile-only job must report a built test executable; every other
non-test job must exit zero and satisfy its allowlisted proof validator.

## Registry

- Records: 1038
- Shards: 32
- Requirements: 346
- Promises: 655
- Decisions: 14
- Risks: 23
- Implemented records: 378
- Proposed records: 586
- Active records: 67
- Optional mechanisms: 35
- Inventory atoms: 1530
- RFC coverage: 1152/1152 nonblank lines, 0 unmapped

## Generated Projections

| Projection | SHA-256 |
| --- | --- |
| `requirements/registry.yaml` | `0a33027604cd27cb4c2bb28952e5ea2c18c185831d7190a43f535df3ddf7bbb1` |
| `requirements/evidence.yaml` | `17999ee38d579be976831083aa67ba06c03dd511eb0648d62401644937f3b522` |
| `requirements/tombstones.yaml` | `fa66d70909259e3fd8ea9d41c7470426352647b104052f0439fbbf8dedf9c29e` |
| `requirements/id-ledger.yaml` | `2bae16ca1a9269994e2f3e20508cca84172361085d64867491d645486bef4265` |
| `requirements/report.md` | `f08fcadc15990954710408ba768fbde95168231fcc208380a47b197f2e2ca335` |

## Evidence Jobs

| Record | Evidence | Job | Command | Artifact SHA-256 |
| --- | --- | --- | --- | --- |
| `KIT-GOV-001` | `EV-1.04-C-001` | `req-lint` | `python3 scripts/req_lint.py --coverage 8-1597` | `11fc57facaf5eefb62a81d424c68c1843cd27d10e09cd03ccca77732d2b239b0` |
| `KIT-GOV-002` | `EV-1.04-C-002` | `req-lint` | `python3 scripts/req_lint.py --aggregate` | `3fe789c292b3887f715e506fe7bacfe85548fa22039ae605b5ac4b58985d1b00` |
| `KIT-ARCH-007` | `EV-1.04-C-003` | `req-lint` | `python3 scripts/check_architecture.py binary` | `06ab5be73dd433acef5ca010a55e9eb8fd04dcdbc669b0eafdf0a5e9b05c1be4` |
| `KIT-ARCH-008` | `EV-1.04-C-004` | `req-lint` | `cargo test --locked --no-run --all-targets` | `ce1233184ee73c31174394d961926dad65c2624d99bc26749e0e468cbe06e805` |
| `KIT-GOV-800` | `EV-1.04-C-800` | `req-lint` | `python3 scripts/generate_registry.py --check` | `ccb75af9f362cabd4378f7f1c4a0477530617467715080a94f68bbb37659a30b` |
| `KIT-GOV-801` | `EV-1.04-C-801` | `req-lint` | `cargo test --locked --test conformance req_lint::req_lint_real_conformance_corpus -- --exact` | `69eae30d5e25b3ebf586fb0e9e7b11f3d183a5ca224aabdcaaa7a54bf4207c1a` |
| `KIT-GOV-802` | `EV-1.04-C-802` | `req-lint` | `cargo test --locked --test conformance req_lint::req_lint_real_conformance_corpus -- --exact` | `bf111eae60ebffbbda6ec0b0bc72a4449b4456b6cce27edf11cbe7a6a26fb7a6` |
| `KIT-GOV-803` | `EV-1.04-C-803` | `evidence-report` | `python3 scripts/req_lint.py --aggregate` | `8b8cb463e04ca103751f76a2e4900d78be844090c518ee497e47710f8f66069c` |
| `KIT-GOV-804` | `EV-1.04-C-804` | `req-lint` | `python3 scripts/req_lint.py --aggregate` | `c519f5cdb3fa380fddc0fde417427a857353d2ee32e07f00be908ac2ae8f5180` |
| `KIT-VERSION-001` | `EV-1.09-C-001` | `schema-compat` | `sh scripts/verify_pins.sh` | `2f8598d9f092f265db873b83db66731f43dead441a57905b7d639dae72dcd5ea` |
| `KIT-VERSION-002` | `EV-1.09-C-002` | `schema-compat` | `sh scripts/verify_pins.sh` | `a2fce7d45148cc4cf34f1be9101da41cf55c1174a7ddc2320d2df69a7964d7f9` |
| `KIT-VERSION-003` | `EV-1.09-C-003` | `schema-compat` | `sh scripts/verify_pins.sh` | `cf3b828248f71abfe274aaa8ae4fc3654d696464be81d29557efb2af310e85e8` |
| `KIT-VERSION-004` | `EV-1.09-C-004` | `schema-compat` | `sh scripts/verify_pins.sh` | `f02fb17a40c458adfcdcc097917ebc099bddb4a6d00f877acee8ff6d0a1835e7` |

Each row binds the exact command, exit code, captured output, artifact digest,
environment text/digest, record ID, evidence ID/job, and worktree identity.

## Reproducibility

- Builder: `darwin-arm64;rustc=1.94.0;cargo=1.94.0`
- Two independent source copies and target directories built with `SOURCE_DATE_EPOCH=0` and `CARGO_INCREMENTAL=0`.
- Both binary SHA-256 values: `7a52ff8f599226bf5d32d95cd08ebb004e98de5faaa0c5873de5187a276f0e66`; `cmp` exited 0.
- Reproducible environment SHA-256: `515e56cd6f55512e88891a48099ac38df0588976409787f0be7b00808b7aace0`.
- Cargo.lock SHA-256: `7943c800482a3961919933ead6b78b69e40859e4f283ff0290489398a543d6c2`.
- Build-input closure SHA-256: `ab0f649a13159fbf4ae7a8a85c53a5d2030116e7cbdbe4d6812e6f5f66628467`.
- `closure_manifest_recorded_post_run=true`: the retained artifact predates embedded closure evidence.
  Its current closure is bound by pre-artifact mtimes and byte-identical retained source copies; future runs record the closure before building and embed its digest.
- Vendored Runlet and Agentkit tests ran with external target directories before snapshot verification.

## Release Gate

Strict release remains closed by unresolved product milestones, pending optional
decisions, non-green dashboards, blocked release pins, the absence of a distinct
ancestor baseline containing a registry, and the absence of external trusted
commit-bound attestations. Release mode rejects this `worktree:` identity and all
source-controlled attestations.
