# M002 Exit Report

- Gate: `G02`
- Milestone: `M002`
- Run date: `2026-07-28`
- Local mechanism result: **PASS**
- Overall result: **BLOCKED_TRANSITIVE (G01)**
- Exit bullets: **10/10 passed locally**
- Evidence commands: **10/10 exited 0; 48 relevant tests passed**
- Candidate identity: `worktree:ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Source-tree SHA-256: `ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Local attestation-set SHA-256: `0d536c1c4eafe5f4c6431a1f081c4031e76d890d8b6cd0f4ebcb16a6c0af9bc2`
- Release result: **FAIL**

All M002 mechanisms pass in this worktree. The retained G00, G01, and G02 local
attestations bind the current source tree, but G02 remains transitively blocked because
G01 is externally blocked. Local G00 reproducibility does not replace release evidence.
Every authoritative Cargo invocation selected at least one passing test.

## Registry

- M002 implemented/mitigated mandatory records: 139
- Requirements: 77
- Promises: 60
- Decisions: 1
- Mitigated risks: 1
- Optional pending-VOI records left pending: 4

## Generated Projections

| Projection | SHA-256 |
| --- | --- |
| `requirements/registry.yaml` | `dbd0965a25758ec9d91937f7d32889c8623c22bcfb26e94ef989399c2ed94c23` |
| `requirements/evidence.yaml` | `bc28e3f6d6fed9c6391ad8f7079dea8da24eb5cc034ddc7ac0a416080cbdf959` |
| `requirements/tombstones.yaml` | `fa66d70909259e3fd8ea9d41c7470426352647b104052f0439fbbf8dedf9c29e` |
| `requirements/id-ledger.yaml` | `2bae16ca1a9269994e2f3e20508cca84172361085d64867491d645486bef4265` |
| `requirements/report.md` | `f08fcadc15990954710408ba768fbde95168231fcc208380a47b197f2e2ca335` |

## Exit Evidence

| Bullet | Dashboard | Disposition | Command | Artifact SHA-256 | Requirement records |
| --- | --- | --- | --- | --- | ---: |
| `IMPLEMENTATION_PLAN.md:366` | `EV-G02-001` | `pass` | `cargo test --locked --test integration cli_daemon::prompt_runs_to_completion_through_daemon_and_cli -- --exact && cargo test --locked --test integration agent_run::agent_run_tests::loopdriver_commits_completion_progress_usage_and_cost -- --exact` | `9b0491c7c119a1ebd940f6fe8a85bdd05966925f557873b1c801214df191152d` | 2 |
| `IMPLEMENTATION_PLAN.md:367` | `EV-G02-002` | `pass` | `cargo test --locked --test fault model_intent_outcome::crash_windows_reconcile_without_duplicate_dispatch_or_invented_success -- --exact` | `f30f39155b0d2db4895722d733dd654b8fcdf6ff59430c44f188080915c99a57` | 3 |
| `IMPLEMENTATION_PLAN.md:368` | `EV-G02-003` | `pass` | `cargo test --locked --test fault loop_restart::every_safe_boundary_restarts_without_duplicate_provider_or_transcript_items -- --exact && cargo test --locked --test fault provider_interrupt::input_approval_and_auth_interruptions_survive_100_restarts_each -- --exact` | `7b833cf24e00428b0ffa759304fc02a24846b2e8ee29dbce51632df6b39db0dc` | 17 |
| `IMPLEMENTATION_PLAN.md:369` | `EV-G02-004` | `pass` | `cargo test --locked --test conformance sched_budget -- --test-threads=1 && cargo test --locked --test fault sched_crash -- --test-threads=1 && cargo test --locked --test integration agent_run::agent_run_tests::budget_exhaustion_fails_before_provider_dispatch -- --exact` | `cf7b46face1d5af163b15fbe037aa8e726d190b3c259f83c4326b3584c282703` | 0 |
| `IMPLEMENTATION_PLAN.md:370` | `EV-G02-005` | `pass` | `cargo test --locked --test conformance prompt_determinism && cargo test --locked --test conformance context_projection` | `f4a34d963d68bafce52a8e0ba137056a40e36c361ac655804d1f9d3a668707da` | 93 |
| `IMPLEMENTATION_PLAN.md:371` | `EV-G02-006` | `pass` | `cargo test --locked --test conformance usage_reconcile && cargo test --locked --test conformance run_telemetry::unavailable_provider_values_are_explicit_nulls -- --exact && cargo test --locked --test conformance run_telemetry::provider_cache_and_accounting_reconcile_without_inventing_values -- --exact` | `514d97807e5f4728aa07bdc1299c10d738e04f666f7e270201409308381f3e4b` | 9 |
| `IMPLEMENTATION_PLAN.md:372` | `EV-G02-007` | `pass` | `cargo test --locked --test fault loop_restart::input_approval_and_auth_waits_survive_and_require_authenticated_resolution -- --exact && cargo test --locked --test integration agent_run::agent_run_tests::approval_and_auth_resolutions_resume_real_waiting_paths -- --exact` | `9798f6138dcec703ac218ced07f6721a0a0175087480a6f43c19f5cdb311dd3a` | 1 |
| `IMPLEMENTATION_PLAN.md:373` | `EV-G02-008` | `pass` | `cargo test --locked --test integration agent_run::agent_run_tests::loopdriver_commits_completion_progress_usage_and_cost -- --exact && cargo test --locked --test integration provider_stream::durable_stream_suppresses_reasoning_and_redacts_secret_forms_and_headers -- --exact && cargo test --locked --test conformance run_telemetry::private_reasoning_has_no_schema_path_and_summary_requires_retention -- --exact` | `3781cdc501ab89f30a696122427cb3e3f019922415ba6cfbdf959c8333037b5c` | 3 |
| `IMPLEMENTATION_PLAN.md:374` | `EV-G02-009` | `pass` | `cargo test --locked --test integration provider_stream::durable_stream_suppresses_reasoning_and_redacts_secret_forms_and_headers -- --exact && cargo test --locked --test conformance run_telemetry::provider_canaries_are_absent_on_every_capture_boundary -- --exact && cargo test --locked --test adversarial secret_leak::persistent_capture_boundaries_remove_raw_and_encoded_canaries -- --exact` | `a6881e2fd7b85d96a4abc65b99832e072e01e76a6f592d79ac841a784d6e6ae1` | 0 |
| `IMPLEMENTATION_PLAN.md:375` | `EV-G02-010` | `pass` | `sh scripts/verify_pins.sh && cargo test --locked --test conformance ext_m002 && python3 scripts/req_lint.py --aggregate` | `6ab8a53e7c29631c8f56103fba29137ed593274bcaa38bd9d78fb8d2e3cbd9e0` | 11 |

Every attestation binds the current worktree/base commit, dashboard row, literal
command and captured output, artifact/environment/version metadata, and each
implemented M002 record's ID, evidence ID, and evidence job.

## Transitive Blockers

- G00, G01, and G02 local attestations are current source-controlled evidence only.
- G01 remains `BLOCKED_EXTERNAL` on `EXT-05` and `EXT-08`.
