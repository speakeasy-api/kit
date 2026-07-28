# M004 Exit Report

- Gate: `G04`
- Milestone: `M004`
- Run date: `2026-07-28`
- Local source/mechanical verdict: **PASS_LOCAL**
- Trusted production verdict: **BLOCKED_EXTERNAL (EXT-01, EXT-04, EXT-15, EXT-22)**
- Overall verdict: **IN_PROGRESS (G02/G03 transitive; EXT-19/EXT-20 transitive)**
- Exit bullets: **10/10 exercised locally; 7/10 passed locally; 2/10 blocked externally; 1/10 blocked transitively**
- Evidence commands: **10/10 exited 0; 843 relevant tests passed**
- Candidate identity: `worktree:ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Source-tree SHA-256: `ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Local attestation-set SHA-256: `70d7aedbb63573dcd77885b3c1e3ff86095d26f3334b9ab7a79956fce9f2c5de`
- Release result: **FAIL**

This candidate is an uncommitted worktree. Its local attestations prove source and
mechanical conformance only, are not the immutable candidate-commit evidence required
for release, and cannot substitute for trusted production dogfood, core, or statistics runs.

## Unit Status

| Unit | Status | Evidence scope |
| --- | --- | --- |
| `5.01` | `PASS_LOCAL` | workspace revision source/conformance |
| `5.02` | `PASS_LOCAL` | bounded lexical search source/conformance |
| `5.03` | `PASS_LOCAL` | discover/read/cursor source/conformance |
| `5.04` | `PASS_LOCAL` | edit IR normalization source/conformance |
| `5.05` | `PASS_LOCAL` | path authorization adversarial source/conformance |
| `5.06` | `PASS_LOCAL` | edit validation source/conformance |
| `5.07` | `PASS_LOCAL` | staging/formatter source/conformance |
| `5.08` | `PASS_LOCAL` | recovery crash/cancellation source/conformance |
| `5.09` | `PASS_LOCAL` | verification-profile source/conformance |
| `5.10` | `PASS_LOCAL` | diagnostic feedback source/conformance |
| `5.11` | `PASS_LOCAL` | grammar edit-path source/conformance |
| `5.12` | `PASS_LOCAL` | native capability-kernel adversarial source/conformance |
| `5.13` | `BLOCKED_EXTERNAL` | local dogfood passed; trusted production dogfood absent |
| `5.14` | `BLOCKED_EXTERNAL` | source semantics passed; trusted production core absent |
| `5.15` | `BLOCKED_EXTERNAL` | ConformanceSourceSemantics report passed; ProductionTrusted statistics absent |

## Registry

- M004 records: 91
- Local pass records with current artifact/environment digests: 85
- Trusted-evidence pending records with null artifact/environment digests: 6
- Requirements: 20
- Promises: 67
- Decisions: 1
- Risks: 3

## Generated Projections

| Projection | SHA-256 |
| --- | --- |
| `requirements/registry.yaml` | `7107fdb3da6c99b5a3cef28e38f4f111106a7d7d6d9afcbe05a805250bace068` |
| `requirements/evidence.yaml` | `76548f83b4e3bf21a256eb5c31c8549c35c2185d25e2f355d34e8bb92b6ac9a8` |
| `requirements/tombstones.yaml` | `fa66d70909259e3fd8ea9d41c7470426352647b104052f0439fbbf8dedf9c29e` |
| `requirements/id-ledger.yaml` | `2bae16ca1a9269994e2f3e20508cca84172361085d64867491d645486bef4265` |
| `requirements/report.md` | `f08fcadc15990954710408ba768fbde95168231fcc208380a47b197f2e2ca335` |

## Local Evidence

| Bullet | Dashboard | Disposition | Command | Passed tests | Artifact SHA-256 | Requirement records | Blockers |
| --- | --- | --- | --- | ---: | --- | ---: | --- |
| `IMPLEMENTATION_PLAN.md:449` | `EV-G04-001` | `blocked_external` | `python3 scripts/check_dogfood_harness.py && cargo test --locked --manifest-path dogfood-harness/Cargo.toml local_mechanical_provider_conformance_uses_public_cli_and_http -- --exact && cargo test --locked --manifest-path dogfood-harness/Cargo.toml direct_public_edit_failure_approval_and_artifact_contracts -- --exact` | 2 | `e64900e99d150df978bfb6f158a240470ea1db9aa15f08b3759b05df67194b92` | 1 | `EXT-01, EXT-22` |
| `IMPLEMENTATION_PLAN.md:450` | `EV-G04-002` | `passed` | `cargo test --locked --test fault edit_recovery -- --test-threads=1` | 31 | `f291c2def2ef06dfcb46876da58f5fa604f90f810d4476e7c982432a3f454c81` | 0 | `none (local/mechanical)` |
| `IMPLEMENTATION_PLAN.md:451` | `EV-G04-003` | `passed` | `cargo test --locked --test conformance edit_validate && cargo test --locked --test adversarial path_escape` | 28 | `0d68230d9be9e7b209d51a2b4768960de0fa55ac94c2bef5457eb6a25bc19351` | 1 | `none (local/mechanical)` |
| `IMPLEMENTATION_PLAN.md:452` | `EV-G04-004` | `passed` | `cargo test --locked --test fault edit_recovery::cancellation -- --test-threads=1` | 3 | `2d6d5edf76fa9c913f71c54242ba3f2ed89d689cd50c14b8874cd2fbfc7ef69b` | 0 | `none (local/mechanical)` |
| `IMPLEMENTATION_PLAN.md:453` | `EV-G04-005` | `passed` | `cargo test --locked --test conformance edit_format` | 23 | `67780002d8d3ff2ea716789896256d23209bf606de0b68747e1ff67f5a220c19` | 0 | `none (local/mechanical)` |
| `IMPLEMENTATION_PLAN.md:454` | `EV-G04-006` | `passed` | `cargo test --locked --test conformance verify_profiles && cargo test --locked --test conformance verify_feedback` | 8 | `028a8fb7d25a3d96ab36af19a480bafb0bb1f0fba7428a5699e0043f110e073a` | 3 | `none (local/mechanical)` |
| `IMPLEMENTATION_PLAN.md:455` | `EV-G04-007` | `passed` | `cargo test --locked --test conformance grammar_edit_path` | 7 | `2d9fc3b270b28bbfa729443c03820b103d9df90d136871acf49a2b1945fd375f` | 0 | `none (local/mechanical)` |
| `IMPLEMENTATION_PLAN.md:456` | `EV-G04-008` | `passed` | `cargo test --locked --test adversarial native_tool_bypass && cargo test --locked --test conformance native_tools` | 19 | `14c5c6be8397f49234cd1b9e272f9916fe98d861e72105c80baa63a71fc5189f` | 15 | `none (local/mechanical)` |
| `IMPLEMENTATION_PLAN.md:457` | `EV-G04-009` | `blocked_external` | `cargo test --locked --test conformance harness_selfcheck && KIT_M004_REPORT_DIR=requirements/reports/m004/source-semantics cargo test --locked --test conformance eval_stats_report::eval_stats_report_uses_exact_binary_primary_and_exploratory_point_estimates_only -- --exact && check-jsonschema --schemafile eval/preregistration/schema/v1/preregistration.schema.json requirements/reports/m004/source-semantics/preregistration.json && check-jsonschema --schemafile eval/preregistration/schema/v1/registration.schema.json requirements/reports/m004/source-semantics/registered-preregistration.json && check-jsonschema --schemafile eval/reports/schema/v1/statistical-report.schema.json requirements/reports/m004/source-semantics/statistical-report.json` | 14 | `53c1fd1e2bc7baeaea3780d30df31f8b4bd4bbddac41619b236eb40e3d300c42` | 5 | `EXT-01, EXT-04, EXT-15, EXT-22` |
| `IMPLEMENTATION_PLAN.md:458` | `EV-G04-010` | `blocked_transitive` | `cargo test --locked --test conformance --test integration --test fault --test adversarial -- --test-threads=1` | 708 | `c20956d53013f962e645e2574b32874bc2a38a2a1561353ab9d07c77c44b7e32` | 66 | `G02, G03, EXT-01, EXT-04, EXT-15, EXT-19, EXT-20, EXT-22` |

Every Cargo invocation selected at least one test. Exit-zero output with zero selected tests
is rejected both while writing and while validating retained attestations.

## Statistical Source-Semantics Artifacts

These retained files are labelled `ConformanceSourceSemantics`; they are not production evidence.

| Artifact | SHA-256 |
| --- | --- |
| `requirements/reports/m004/source-semantics/preregistration.json` | `937e928baa846235d171fc857639ed287211dc6467bf07e2a412560180da80f9` |
| `requirements/reports/m004/source-semantics/registered-preregistration.json` | `5ddab025a27d792397eaef265d0c2bcfdf9c52eaf9d4d16ac6b65a92d9903d97` |
| `requirements/reports/m004/source-semantics/statistical-report.json` | `7efacd9c0d4811360b058ca1334dd4fbeca55b9a990401b08325866682643452` |
| `requirements/reports/m004/source-semantics/statistical-report-receipt.json` | `0820471b92b8efa5beb8c10f1f08ce51f044a181e22f3dd93c332b9bd5f55a53` |

## Blockers

- Direct: `EXT-01`/`EXT-22` trusted Linux helper evidence for production dogfood; `EXT-01`/`EXT-04`/`EXT-22` trusted isolated core/statistical execution; `EXT-15` production provider credentials and approved spend for production statistics.
- Transitive: `G02` depends on externally blocked `G01`; `G03` depends on `G01` and remains blocked by `EXT-01`, `EXT-04`, `EXT-19`, `EXT-20`, and `EXT-22`.
- No source-controlled local attestation is accepted by release governance.
