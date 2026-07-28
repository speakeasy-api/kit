# M001 Exit Report

- Gate: `G01`
- Milestone: `M001`
- Run date: `2026-07-28`
- Result: **BLOCKED_EXTERNAL (EXT-05, EXT-08)**
- Exit bullets: **12/15 passed; 3/15 blocked by external prerequisites**
- Evidence commands: **15/15 exited 0; 841 relevant tests passed**
- Candidate identity: `worktree:ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Source-tree SHA-256: `ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Local attestation-set SHA-256: `1795347123267fd371c4ffb5aac75c1a35929d93cb1797ccdccc4006b8f00478`
- Release result: **FAIL**

The local implementation evidence is current for this worktree. Source-controlled
attestations are local-only and are rejected by final release validation.
Every authoritative Cargo invocation selected at least one passing test.

`EXT-05` requires Windows CI for CLI/API parity. `EXT-08` requires a provisioned
OIDC IdP and CA with live issuance and revocation.
The real cryptographic fake-PKI fixture ran successfully but is operational evidence
(`O`), not external conformance (`C`), so it does not close G01. The aggregate exit
bullet remains blocked transitively; no internal G01 blocker is claimed.

## Registry

- M001 implemented/mitigated records: 147
- Requirements: 23
- Promises: 120
- Decisions: 2
- Mitigated risks: 2
- External pending record: `KIT-API-804`
- Future mechanism promises reassigned from M001 primary ownership: 28

## Generated Projections

| Projection | SHA-256 |
| --- | --- |
| `requirements/registry.yaml` | `0a33027604cd27cb4c2bb28952e5ea2c18c185831d7190a43f535df3ddf7bbb1` |
| `requirements/evidence.yaml` | `17999ee38d579be976831083aa67ba06c03dd511eb0648d62401644937f3b522` |
| `requirements/tombstones.yaml` | `fa66d70909259e3fd8ea9d41c7470426352647b104052f0439fbbf8dedf9c29e` |
| `requirements/id-ledger.yaml` | `2bae16ca1a9269994e2f3e20508cca84172361085d64867491d645486bef4265` |
| `requirements/report.md` | `f08fcadc15990954710408ba768fbde95168231fcc208380a47b197f2e2ca335` |

## Exit Evidence

| Bullet | Dashboard | Disposition | Command | Artifact SHA-256 | Requirement records |
| --- | --- | --- | --- | --- | ---: |
| `IMPLEMENTATION_PLAN.md:322` | `EV-G01-001` | `pass` | `cargo test --locked --test conformance store_append::sixty_four_real_connections_allocate_one_gapless_committed_prefix -- --ignored --exact --test-threads=1` | `d3613e3d38c6634d4eac6975c4fd40a980c2fac7efacd741e02cd55f3d864b23` | 9 |
| `IMPLEMENTATION_PLAN.md:323` | `EV-G01-002` | `pass` | `cargo test --locked --test conformance store_projection::replay_is_byte_identical_across_twenty_restarts -- --exact` | `3ac426aba5c9bb71f055f418b95011bbfce64f7f8e7a099e7e0d0c4625686bda` | 3 |
| `IMPLEMENTATION_PLAN.md:324` | `EV-G01-003` | `pass` | `cargo test --locked --test conformance store_append::idempotency_replays_only_the_same_canonical_request_and_exposes_pending -- --exact && cargo test --locked --test conformance deletion_api` | `f7fb1ba6cd4db6a8d632569d39b2f32f58873d830f10632e46d4b61130577d3b` | 8 |
| `IMPLEMENTATION_PLAN.md:325` | `EV-G01-004` | `pass` | `cargo test --locked --test conformance config_layering` | `f00bfd9a459cc0bf61dadee1f2f05819cb86c2614cfac21b882ff082b79dfeac` | 7 |
| `IMPLEMENTATION_PLAN.md:326` | `EV-G01-005` | `pass` | `cargo test --locked --test conformance sched_budget -- --test-threads=1 && cargo test --locked --test fault sched_crash -- --test-threads=1` | `e29dfcf2a5f8a4164544b9757f863a59f11c3321ac9935712ca885def102efa5` | 2 |
| `IMPLEMENTATION_PLAN.md:327` | `EV-G01-006` | `pass` | `cargo test --locked --test conformance cap_invoke && cargo test --locked --test adversarial cap_bypass` | `44773245759d3cd494e0c1b164bd215b4e757daf64dcb86c83ab89d7347fa2a7` | 2 |
| `IMPLEMENTATION_PLAN.md:328` | `EV-G01-007` | `pass` | `cargo test --locked --test fault fencing && cargo test --locked --test fault lifecycle_cas` | `0709258284f48275cdcdfa10bc9a09906abe3eb115c02e908308bb8f40f5d19d` | 47 |
| `IMPLEMENTATION_PLAN.md:329` | `EV-G01-008` | `pass` | `cargo test --locked --test fault artifact_crash` | `b54c37bc629c69a4953bc193cdf2a084416458e69b5a091f8cd6d00138b88eea` | 6 |
| `IMPLEMENTATION_PLAN.md:330` | `EV-G01-009` | `pass` | `cargo test --locked --test conformance sse_cursor` | `9de562f48a7882964d6171b6d6d87d8a72b37a3d2d816b8593d7cd9020f57ae5` | 6 |
| `IMPLEMENTATION_PLAN.md:331` | `EV-G01-010` | `pass` | `cargo test --locked --test conformance sse_cursor::cross_principal_and_nonexistent_streams_are_byte_identical -- --exact && cargo test --locked --test adversarial auth_local::authorization_denials_do_not_disclose_cross_resource_state -- --exact && cargo test --locked --test conformance deletion_api::http_returns_jobs_typed_hold_refusal_and_no_cross_principal_details -- --exact` | `fbbf1f0582d4c9403e60184c389963a5acd6407abaa12013d36a017a57c50159` | 1 |
| `IMPLEMENTATION_PLAN.md:332` | `EV-G01-011` | `pass` | `cargo test --locked --test integration backup_restore && cargo test --locked --test conformance retention_model && cargo test --locked --test conformance deletion_api` | `788ebc41bd6243a1296107240ca43213748d7a93e3379d19c9175097a16271e4` | 11 |
| `IMPLEMENTATION_PLAN.md:333` | `EV-G01-012` | `blocked_external` | `cargo test --locked --test adversarial auth_local::exact_seven_required_denial_cases_are_closed -- --exact && cargo test --locked --test adversarial auth_local::readiness_requires_both_components_in_every_boot_order -- --exact && cargo test --locked --test adversarial auth_remote::operational_fake_pki_denies_exactly_seven_required_cases_and_accepts_valid_peers -- --exact` | `685374197b8fa64e2d30f17526c80ec4be7bbd5f215c4f4134023d410ee178a1` | 2 |
| `IMPLEMENTATION_PLAN.md:334` | `EV-G01-013` | `pass` | `cargo test --locked --test conformance telemetry_export && cargo test --locked --test adversarial secret_leak` | `02c3a4171c246e930ec0cd676cb7d7a31e7702d770bdecac636655d1e411997d` | 3 |
| `IMPLEMENTATION_PLAN.md:335` | `EV-G01-014` | `blocked_external` | `python3 -m openapi_spec_validator docs/api/openapi.yaml && cargo test --locked --test conformance cli_parity && cargo test --locked --test conformance handler_parity && cargo test --locked --test conformance http_contract` | `4f48b3ed7f1cb20edd34bebabe34ddfa3d9be5dd194f996c9da66807898bb549` | 20 |
| `IMPLEMENTATION_PLAN.md:336` | `EV-G01-015` | `blocked_external` | `cargo test --locked --test conformance --test integration --test fault --test adversarial -- --test-threads=1` | `080bb6722b8bf6b37f3a85d919c7e8aeca82a38db8a695d07d6fd2deab7b4b62` | 20 |

Every attestation binds the current worktree/base commit, dashboard row, literal
command and captured output, artifact/environment/version metadata, and each
requirement record's ID, evidence ID, and evidence job. The report binds the full set.

## External Blocker

G01 remains `BLOCKED_EXTERNAL` until `EXT-05` supplies Windows CLI/API parity
and `EXT-08` supplies live IdP/PKI issuance, validation, and revocation evidence.
The operational cryptographic fixture cannot be relabeled as conformance evidence.

## Build Provenance Limitation

The retained reproducible artifact is bound to build-input closure `ab0f649a13159fbf4ae7a8a85c53a5d2030116e7cbdbe4d6812e6f5f66628467`
using pre-artifact mtimes and byte-identical retained source copies. The closure manifest was
recorded after the run (`closure_manifest_recorded_post_run=true`), so this is not equivalent
to a closure digest embedded by the completed run. Future reproducible builds embed that digest.
