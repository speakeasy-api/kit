# Verification Feedback

M004 feedback schema v1 turns authenticated verification results into bounded model-facing deltas,
full diagnostic reports, and durable check events. It consumes only stdout, stderr, and process
reports already sanitized and authenticated by `CheckRunner`; a second redaction pass must produce
identical bytes before parsing. Raw process output has no feedback persistence path.

Verification result payload construction and persistence are sealed inside `verify_precommit`.
Each canonical result and receipt includes random executor-minted provenance bound to its stage,
selected plan, and authority; callers can validate but cannot construct or deserialize a result.

## Red Baseline

The feedback baseline primitive captures the pre-edit workspace revision before staging. The
returned `BaselineCapture` is authority-bound and cannot be constructed by callers. Post-edit
processing accepts it only for the same run/edit/fence authority and a different workspace revision.
Compatibility requires the same ordered check IDs, executor contract version, trusted diagnostic
adapter/version, measured image/tool digest, and effective config digest. Missing, late, malformed,
or incompatible evidence is explicit. Current diagnostics then have `observed` status, never `new`,
so feedback cannot claim an edit caused a pre-existing failure.

Available baselines produce deterministic `new`, `resolved`, `persisting`, and `changed` deltas.
Path moves and bounded line moves map baseline coordinates into the post-edit tree before matching.
Every result records whether its post-edit range intersects a declared changed-line range.
Callers cannot supply a mapping: it is derived from validated effects and the exact frozen staged
bytes, validated for injective paths and non-overlapping checked ranges, and included in the staged
changes digest.

## Diagnostics And Bounds

Trusted v1 adapters accept normalized JSON Lines and rustc `compiler-message` JSON. Stable identity
is check, root-relative path, one-based range, code, BLAKE3 message digest, severity, and tool.
Paths, messages, records, input bytes, diagnostic count, report bytes, feedback bytes, artifact
references, memory, and total operation time have independent hard limits. One aggregate input,
diagnostic, memory, and time budget spans baseline and current streams. Oversized and malformed
records are counted without retaining their contents; a zero remaining diagnostic budget still
accepts empty or malformed-only input, while the next valid diagnostic returns a typed limit error.
Sorting and ranking are deterministic: required failures precede new, changed, observed,
persisting, and resolved diagnostics, then severity, changed-line attribution, and stable identity.

The canonical feedback payload reports exact sanitized input, baseline diagnostic, current
diagnostic, total result, included result, omitted result, and serialized byte counts. Truncation
removes only the lowest-ranked diagnostics and sets an explicit marker. A required check failure is
never truncated. If the fixed envelope and required failures do not fit, the operation fails rather
than returning an incomplete failure.

## Artifacts And Events

Full sanitized stdout and stderr remain the authenticated `Log` artifacts recorded by the
verification receipt. Feedback stores a separate complete canonical diagnostic/delta report and the
bounded payload as authenticated `Report` artifacts. Model-facing data contains only opaque artifact
references and lengths. The complete report is independent of feedback truncation.

The authority-gated event journal emits schema-v1 `check.started`, `check.progress`, and one terminal
`check.completed` or `check.failure`. Each event binds principal, project, workspace, run, edit
digest, workspace revision, verification plan/result digests, fence, check, status, diagnostic
count, and artifact references. Runner observer hooks durably record actual lifecycle boundaries;
restart reconciliation publishes only terminal lifecycles backed by a canonical
`VerificationResult`, never an invented success. SQLite WAL/FULL transactions look up the canonical
idempotency key and full payload digest before allocating a per-authority gapless cursor. Duplicate
delivery replays the original event, conflicting payloads fail, and restart reads preserve cursor
order. Reads require an authorized `AuthenticatedPrincipal`, filter the feed, and validate every
artifact reference and manifest before returning it. Generic unauthenticated reference resolution is
crate-private.

Before publication, SQLite stores a canonical pending feedback record containing the authenticated
result receipt, baseline reference and compatibility, canonical edit mapping, adapter and limit
identity, deterministic report and payload references, and the expected event sequence. Recovery
uses only that record and authenticated artifacts; no live staged edit or verification outcome is
required. Mapping and record JSON are counted without an output allocation before bounded
serialization, and the complete canonical bytes must be unchanged by the capture redactor before
SQLite is touched. Recovery checks pending count and aggregate bytes first, gates each BLOB with
SQLite `length()`, and processes one bounded row at a time. Noncanonical, malformed, or secret-bearing
rows are quarantined without publishing an event. Observer lifecycle rows use no public feed cursor.
A cursor is allocated only in the same immediate transaction as an immutable event insert, so replay
and recovery cannot leave holes.
Materialization appends a separate `feedback.successor_attached` event; it never rewrites prior event
bytes or IDs.

## Production Orchestration

M004-W11 wires the production `EditOperationContext`, live `MutationGuard` revalidation, stable
baseline, stage/result/successor provenance, and this feedback pipeline into both agent and direct
repository calls. The public result retains bounded feedback plus references to complete reports,
events, verification evidence, cost, and the actual diff.

## Evidence

- `cargo test --locked --test conformance verify_feedback`
- `cargo test --locked --lib verify::feedback::tests`
- `cargo test --locked --lib workspace::edit::stage::unix::verification_tests::real_process_death_recovers_feedback_without_outcome_or_cursor_holes -- --exact --test-threads=1`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `python3 scripts/req_lint.py --aggregate`
