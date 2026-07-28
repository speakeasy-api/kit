# Release operation

Kit releases are gated before publication. A published GitHub release is never
used as evidence that its own candidate passed.

1. Produce release evidence in trusted CI for the exact candidate commit. Each
   JSON attestation must bind one evidence ID and job to the candidate commit,
   workflow ref, run ID, artifact digest, and environment digest.
2. Configure `KIT_TRUSTED_ATTESTATION_SHA256` with the allowed attestation file
   digests. Source-controlled files under `requirements/attestations/` are local
   G00 records and must not be included.
3. Protect the default branch, every release `v*` tag, and the
   `release-validation` and `release-publish` environments. Candidate authors
   must not be able to alter the default-branch workflow, approve either
   environment, or alter required checks.
4. Dispatch `CI` from the protected default branch with `candidate_ref`,
   `baseline_ref`, `attestation_run_id`, `attestation_artifact`, and
   `attestation_workflow_ref` explicitly set. The baseline must be a protected
   Git ref or SHA; publication additionally requires `candidate_ref` to be a
   protected `refs/tags/v*` ref.
5. Wait for all 12 candidate lanes, then `release-validate`. The release jobs
   require the workflow identity to be the protected default branch. Validation
   executes tools from a validator checkout pinned to that workflow commit
   against a separate candidate checkout, resolves candidate and baseline to
   explicit SHAs, requires a distinct ancestor baseline with a nonempty
   registry, runs strict
   `verify_pins.py --release`, requires green dashboards, and validates the
   downloaded external attestations.
6. The dependent `publish` job creates the GitHub release only for the protected
   `v*` candidate tag and only after all lanes and `release-validate` pass. Its
   sole elevated permission is `contents: write`; validation has read-only access.

The strict command intentionally fails when the baseline is empty or cannot be
resolved, the external bundle is absent or untrusted, any attestation describes
an unrelated evidence ID/job/provenance tuple, or product evidence remains
pending.

`--baseline-file` is only for explicit local mutation tests and is forbidden in
release mode. Source-controlled attestations and `worktree:<digest>` candidate
identities can establish local G00 only; final release requires external,
trusted, commit-SHA-bound attestations.
