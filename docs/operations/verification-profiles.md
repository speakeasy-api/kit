# Verification Profiles

M004 verification profile contract v2 has exactly five profiles: `none`, `syntax`, `fast`,
explicit `targeted`, and explicit `full`. A plan is canonical JSON identified by a BLAKE3 digest and
binds the workspace revision, edit transaction, staged or materialized state, immutable source-tree
digest, changed paths, project check declarations, authority, timing, and finite aggregate budget.
Reordering project declarations cannot change the plan or digest.

## Selection

- `none` selects no evidence-producing check and records `skipped=true`.
- `syntax` consumes the authoritative syntax/formatter evidence produced while staging. Missing,
  unavailable, non-authoritative, or stale evidence aborts.
- `fast` deterministically selects bounded project-declared fast checks whose canonical path prefixes
  cover a changed path. It does not infer commands.
- `targeted` requires explicit authority and a non-empty exact set of declared targeted check IDs.
  It first selects every affected required `syntax`, `diagnostics`, and `typecheck` declaration from
  the trusted registry using the staged changed paths, then adds the exact targeted IDs. An unknown
  ID aborts planning, and any floor failure aborts publication.
- `full` requires explicit full-check authority and an explicitly supplied finite budget. It selects
  every declared check.

Every selected plan is ordered by class (`syntax`, `diagnostics`, `typecheck`, `targeted`, `full`)
and then lexical check ID. Callers cannot reorder the plan with target-set ordering.

Duplicate IDs, unsafe path prefixes, unpinned images, missing tool/config digests, NUL-bearing argv,
zero bounds, aggregate overflow, and over-budget plans are rejected. Arguments remain an argv vector
throughout planning and execution; no shell string is generated. Declarations can only be constructed
by the trusted in-crate measured-executable registry. The registered program is the exact executable
passed to the sealed helper under its sandbox; arguments, including shell metacharacters and any
interpreter arguments, remain literal argv entries. No project-provided PATH lookup, command string,
or inferred executable is trusted.

## Commit Matrix

The required-check matrix is fixed. `none/fail` means an unconsumed hypothetical check result, not a
failed required check.

| Profile | Required pass | Required fail |
| --- | --- | --- |
| `none` | commit, skipped evidence | commit, skipped evidence |
| `syntax` | commit | abort |
| `fast` | commit | abort |
| explicit `targeted` | commit | abort |
| explicit `full` | commit | abort |

`on_check_failure: commit` is accepted only when every selected failure is advisory. A required
failure always aborts and cannot be promoted by selection or policy. An accepted advisory failure is
stored as `accepted_failure=true` with its non-pass result and evidence; it is never rewritten as a
pass. Syntax and fast pre-commit hard gates cannot use commit-on-failure.

The implemented pre-commit path is `StagedEdit::verify`: abort-on-failure plans run only against an
immutable staged tree before materialization. Every run
rechecks the complete tree digest before and after each external process and rejects a changed
revision, transaction, state, or source digest. The implementation validates post-commit policy and
records `AlreadyCommittedWithFailure`; dispatch of a future committed event is intentionally not
claimed here. Such execution requires a named policy,
explicit post-commit authority, materialized-state binding, and checks declared advisory and
post-commit safe. Since the revision is already accepted, its failure is recorded and is never
silently represented as an undo or successful verification.

## Receipt And Recovery

Before the first workspace mutation, the canonical `VerificationResult` payload is stored as an
owned `Report` artifact. Its digest covers the ordered selected checks and outcomes, full stream
identities, and every process report's opaque reference, content digest, length, and cancellation
identity. A separate evidence digest is recomputed from that exact ordered check evidence.

The recovery manifest stores the exact v2 `VerificationReceipt`: schema, expected plan, stage
binding, result and evidence digests, the result report reference and length, every opaque
stdout/stderr artifact reference and length, and one ordered process entry per selected check.
Prelaunch checks are explicit `not_started` entries; launched checks require a `report` entry.
`MaterializedEdit::verification_receipt` exposes that same receipt for a future committed event.
Live materialization and startup rollback/rollforward resolve and parse the canonical result bytes,
recompute both digests, compare the expected plan, binding, and authority, then resolve every listed
stream and process report. Missing, duplicate, reordered, substituted, or extra evidence fails
closed. While recovery is pending, transaction leases pin the complete verification artifact set,
including opaque reference metadata; final cleanup releases those leases only after recovery no
longer needs them.

Artifact references are independent retention/ownership records, not content identities. Equal
bytes share one BLAKE3 content object while each opaque reference independently binds principal,
project, class, media type, retention, and storage day. GC keeps content while any reference,
lease, owner, hold, backup, or reachability edge remains live.

## Executor Boundary

Every external check uses `executor::check::CheckRunner`. The production route is the sealed M003
`kit-container-v1` helper; there is no host-process route and callers cannot inject an executor
implementation. The runner creates a restricted profile with:

- read-only immutable source and writable build/temp mounts only;
- no credentials and no repository hook/submodule execution;
- network deny at the helper and runtime layers;
- finite aggregate CPU, memory, PID, file, disk, I/O, output, and wall-time limits;
- a pinned image plus measured tool artifact and effective configuration digests;
- bounded, redacted stdout/stderr previews and content-addressed full-stream references;
- persisted plan/invocation/process evidence, durable cancellation evidence, whole-boundary kill,
  and zero-survivor quiescence before completion is accepted.

Prelaunch rejection, unavailable helper, or spawn failure is `not_started` and carries no helper,
runtime, or quiescence claim. Timeout, cancellation, nonzero completion, and protocol failure after
launch retain actual process kill/reap/inspection evidence. Missing post-launch process evidence is
`not_quiescent`; `CheckRunner` never synthesizes a sealed-helper route or runtime identity.

Unknown checks, nonzero exits, timeout, cancellation, protocol mismatch,
output-bound failure, stale trees, and absent quiescence are explicit non-pass outcomes. Required
instances abort.

## Public Surface

Production API, CLI, and agent reachability is provided by M004-W11 through the authenticated
repository API. Direct and agent checks use this registry and the same native capability path;
missing sealed execution remains a typed tool-specific unavailability, never a host fallback.

Red-baseline capture, diagnostic comparison, bounded failure payloads, complete diagnostic reports,
and durable check-event semantics are specified in `docs/operations/verification-feedback.md`.

## Evidence

- `cargo test --locked --test conformance verify_profiles`: exactly 2 passed, 0 failed.
- `cargo test --locked --lib verify::profiles::tests`: exactly 6 passed, 0 failed, 1 ignored unless
  the external trusted-helper environment is supplied.
- `cargo test --locked --test conformance artifact_gc`: exactly 6 passed, 0 failed.
- `cargo test --locked --test fault edit_recovery::rollback_and_rollforward_preserve_the_exact_verification_receipt -- --exact`:
  exactly 1 passed, 0 failed.
