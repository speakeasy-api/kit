# Agentkit Post-Validation Checkpoint Hook

- Work package: `M009-W01`
- Agentkit release: `0.10.2`
- Patch ID: `m009-post-validation-checkpoint`
- Snapshot SHA-256: `5bf963f65dcab767a1585a45bf4fbdd21c56dbbb57ea936727825d5809e11dc4` (current payload after the local M006-W08 URL-capability overlay; `M006-W07c` produced `0f8daf8b32585f2900ffa0f2c15084dccb0d1f3bc312919906f710f3b8f85af0`, `M006-W07b` produced `3cc4569be6990cd88265f9e3d5d2c057c1cfd4eefad5da4ff0ece4150d758077`, and `M006-W07a` produced `0b10acaf53d52a4aa6cbfd183366de7bb401cf2d194efb239fddeed585c419c2`)
- Upstream status: pending submission; upstream acceptance is non-blocking
- Semantic compaction: disabled until `M009-W06`

## Ordering Contract

Dirty mutator passes use a copy-on-write transcript candidate when the hook is installed. Agentkit
runs every mutator, validates transcript invariants, and then invokes the sole fallible checkpoint
hook. Only a `Committed` outcome promotes the exact candidate into the live transcript. Model dispatch
occurs after promotion. `NotCommitted` leaves the base transcript live, and `Unknown` retains the exact
operation ID, base, candidate, turn, and original cancellation checkpoint for reconciliation.

Clean mutation passes do not create checkpoints. Kit's existing attempt-owned `ModelAdapter` remains
the dispatch fence for every model call, including clean passes; this hook does not create a second
dispatch authority.

## Authority And CAS

The durable host supplies `PostValidationCheckpointCursor` from authoritative state. Its attempt ID,
fence, and unique driver lease ID identify the writer. The operation sequence identifies each new
candidate and is never reused. The durable-head sequence and exact base transcript identify the
expected parent. A hook implementation must hash its base and candidate canonically and atomically
compare all of these values before returning `Committed`:

- attempt ID;
- attempt fence;
- unique driver lease ID;
- checkpoint operation sequence;
- expected previous durable-head sequence;
- exact base transcript digest;
- exact candidate transcript digest.

A checkpoint-enabled `Agent` starts at most one driver. A restarted host constructs a new cursor from
the durable head and a fresh fenced driver lease. `Committed` on an idempotent retry is valid only for
the same complete checkpoint identity and candidate.

## Cancellation And Retry

Cancellation is checked after mutation and validation but before initial checkpoint submission. Once
submission begins, commit reconciliation is logically uncancellable: dropping the `next()` future or
receiving `Unknown` preserves the pending operation, and the next call reconciles that same operation
before any model dispatch. After authoritative resolution, the original turn cancellation still wins
before model or tool dispatch. Cancellation also wins a resolved-approval race.

The cursor advances operation IDs when candidates are created, while its durable head advances only
on `Committed`. Gaps therefore represent known-abandoned operations and do not weaken the explicit
expected-head CAS.

## Compatibility

Without a hook, the existing Agentkit mutation path remains in place. `TranscriptCursor::replace`
lets whole-transcript compactors install a candidate without cloning the prior transcript; in-place
mutators retain copy-on-write behavior when checkpointing is enabled.

`M009-W02` owns durable candidate, validated, rejected, and promoted event/artifact states. `M009-W06`
owns the Kit hook implementation and atomic production installation. No production semantic
compaction or restart-from-checkpoint claim is made by W01.

The exact changed-file hashes and prior/current aggregate snapshot digests are recorded in
`src/agent/agentkit_patch/manifest.yaml`. The vendored snapshot manifest is the reproducible patch
payload. Upstream submission remains tracked but does not block the Kit-owned pinned overlay.
