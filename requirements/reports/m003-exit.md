# M003 Exit Report

- Gate: `G03`
- Milestone: `M003`
- Run date: `2026-07-28`
- Local source/conformance result: **PASS_LOCAL**
- Overall result: **BLOCKED_EXTERNAL (EXT-01, EXT-04, EXT-19, EXT-20, EXT-22; G01/G02 transitive)**
- Exit bullets: **10/10 exercised locally; 8/10 blocked externally; 1/10 in progress; 1/10 blocked transitively**
- Evidence commands: **10/10 exited 0; 199 relevant tests passed**
- Candidate identity: `worktree:ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Source-tree SHA-256: `ba08b835fcbb9348cde255d803db050fb56986e61a9366327930a0898be34306`
- Local attestation-set SHA-256: `1c9bcc317da20659513d34dad6028c141e40f165aec45345791fc038094ab8fb`
- Release result: **FAIL**

These attestations bind the current uncommitted worktree and prove only local source and
conformance behavior. They are not trusted external runtime attestations and cannot pass
Linux helper/cgroup/network/filesystem/daemon-SIGKILL, Windows, macOS VM, or architecture cells.

## Registry

- M003 active records: 39
- Requirements: 7
- Promises: 29
- Decisions: 1
- Risks awaiting mitigation evidence: 2
- Record evidence disposition: `latest_result: pending`; no M003 record is release-closed by local evidence.

## Generated Projections

| Projection | SHA-256 |
| --- | --- |
| `requirements/registry.yaml` | `dbd0965a25758ec9d91937f7d32889c8623c22bcfb26e94ef989399c2ed94c23` |
| `requirements/evidence.yaml` | `bc28e3f6d6fed9c6391ad8f7079dea8da24eb5cc034ddc7ac0a416080cbdf959` |
| `requirements/tombstones.yaml` | `fa66d70909259e3fd8ea9d41c7470426352647b104052f0439fbbf8dedf9c29e` |
| `requirements/id-ledger.yaml` | `2bae16ca1a9269994e2f3e20508cca84172361085d64867491d645486bef4265` |
| `requirements/report.md` | `f08fcadc15990954710408ba768fbde95168231fcc208380a47b197f2e2ca335` |

## Local Evidence

| Bullet | Dashboard | Disposition | Command | Passed tests | Artifact SHA-256 | Requirement records | Blockers |
| --- | --- | --- | --- | ---: | --- | ---: | --- |
| `IMPLEMENTATION_PLAN.md:407` | `EV-G03-001` | `blocked_external` | `cargo test --locked --test adversarial local_sandbox && cargo test --locked --test adversarial container_fs && cargo test --locked --test adversarial container_net` | 17 | `839d2b4a5afbb3e5edc256fec7545352323699d78c2d1cd3241b572631cfa918` | 5 | `EXT-01, EXT-04, EXT-19, EXT-20` |
| `IMPLEMENTATION_PLAN.md:408` | `EV-G03-002` | `blocked_external` | `cargo test --locked --test conformance container_limits && cargo test --locked --test conformance process_output` | 14 | `4e09ae3d464d9605ca08f845794a947417c90a3973b1ae0f7b6f4bd2bfcc8d2b` | 3 | `EXT-01, EXT-04, EXT-19, EXT-20` |
| `IMPLEMENTATION_PLAN.md:409` | `EV-G03-003` | `blocked_external` | `cargo test --locked --test fault process_reap && cargo test --locked --test fault exec_cancel` | 18 | `795cf89d3f9814a2d6bce83aa64ff27ecce54fcf6b6036cd209caaf94f7275d1` | 6 | `EXT-01, EXT-04, EXT-19, EXT-20, EXT-22` |
| `IMPLEMENTATION_PLAN.md:410` | `EV-G03-004` | `blocked_external` | `cargo test --locked --test conformance exec_profile && cargo test --locked --test adversarial local_sandbox` | 15 | `dd7e55d7e9789a4b884e8dea8c3eb1c8bdb64ce2e3755e6715c8cf5579fb825e` | 6 | `EXT-01, EXT-04, EXT-19, EXT-20` |
| `IMPLEMENTATION_PLAN.md:411` | `EV-G03-005` | `in_progress` | `cargo test --locked --test conformance workspace_acquire` | 24 | `b03f10f5ace4c4eeadcf553c15d0a2a4563c066b3de9d1dfcc74ff64c79463f3` | 5 | `uncommitted/in progress` |
| `IMPLEMENTATION_PLAN.md:412` | `EV-G03-006` | `blocked_external` | `cargo test --locked --test fault exec_cancel && cargo test --locked --test fault fencing` | 16 | `6f5393e8c413848c677051ccbaca848669bcc722dc3b4ead1c6b0d0b8085181c` | 1 | `EXT-01, EXT-04, EXT-19, EXT-20, EXT-22` |
| `IMPLEMENTATION_PLAN.md:413` | `EV-G03-007` | `blocked_external` | `cargo test --locked --test adversarial exec_secret_leak && cargo test --locked --test conformance terminal_lease::secret_absent_terminal_history -- --exact` | 4 | `0c00a4976761cb79854a3e9487ed11703de041a3e10316a560d5ce904fbeeba6` | 7 | `EXT-01, EXT-04, EXT-19, EXT-20` |
| `IMPLEMENTATION_PLAN.md:414` | `EV-G03-008` | `blocked_external` | `cargo test --locked --test conformance exec_api && cargo test --locked --test conformance terminal_lease` | 32 | `6ac69a1483af0f9598384547c3bdaadb14e90e452a395b4892d853a0c882bca6` | 4 | `EXT-19, EXT-22` |
| `IMPLEMENTATION_PLAN.md:415` | `EV-G03-009` | `blocked_external` | `cargo test --locked --test adversarial trial_grader_access` | 11 | `384ef5b6823f660444c2b11abeb45af0c9bc5f5e91b13a558b40d969bdacf75a` | 0 | `EXT-01, EXT-04, EXT-20` |
| `IMPLEMENTATION_PLAN.md:416` | `EV-G03-010` | `blocked_transitive` | `cargo test --locked --test conformance exec_contracts && cargo test --locked --test conformance exec_api && cargo test --locked --test adversarial trial_grader_access && cargo test --locked --test fault exec_cancel` | 48 | `6853d0d289a24fb13ef100ef50d570cc795fc1ccc92f27aa20084f757186137a` | 2 | `G01, G02, EXT-01, EXT-04, EXT-19, EXT-20, EXT-22` |

Each command selected at least one relevant test in every Cargo invocation. The validator
rejects exit-zero output containing zero selected tests and rechecks the captured count.

## Blockers

- `EXT-01`: trusted Linux x86_64 cgroup v2, Landlock, helper, filesystem, network, limits, and process runtime artifacts are absent.
- `EXT-04`: the equivalent Linux aarch64 runtime artifacts are absent.
- `EXT-19`: Windows Job Object, ConPTY, trusted runtime helper, and isolation-provider artifacts are absent.
- `EXT-20`: macOS per-run VM escape and zero-survivor artifacts are absent.
- `EXT-22`: production attempt-owned PTY helper daemon-SIGKILL/restart evidence does not yet exist.
- `G01` remains blocked externally and `G02` remains blocked transitively; G03 cannot release over either dependency.

No local source result is relabeled as external conformance, and no release PASS is claimed.
