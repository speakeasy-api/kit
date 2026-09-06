# Intentional panic policy

> **Published as a blocked draft — migration is incomplete, not merge-ready.**
> The proposed enforcement still rejects the session persistence callback and
> a test-only policy inconsistency. These are blockers, not grandfathered
> exceptions. Do not merge or weaken the rules to conceal them.

This policy rejects explicit panic mechanisms in the Rust library, binary, and
build script. It is a finite Clippy policy, **not proof of panic-freedom**:
indexing, arithmetic, allocation, dependency/library APIs, unsafe code, and
compiler or macro behavior remain separate audits. `dbg!` is outside this scope.

## Enforcement

`Cargo.toml` denies `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`,
`unimplemented`, `disallowed_methods`, and `disallowed_macros`. The library,
binary, and build-script roots also use literal `cfg_attr(not(test), forbid(...))`
attributes. Production code cannot undo that rule with a local `allow`.

`clippy.toml` disallows Option's `unwrap`/`expect` and Result's
`unwrap`/`expect`/`unwrap_err`/`expect_err` by their canonical `core` paths.
This covers UFCS calls and function-item references, not just method calls.
`std::panic::panic_any` is also disallowed, including function-item references.
Macro restrictions cover core/std panic, assertions, debug assertions,
unreachable, and placeholders, including tested local wrappers that ordinary
restriction lints miss.

Const unwrap/expect are **not** exempt. For deliberate compile-time validation,
use `compile_error!` under an appropriate configuration condition. Build inputs
are fallible runtime inputs: validation must return a reported error from
`main`, not panic or assert.

Test ergonomics are scoped to test-only modules or integration crate roots;
production modules have no such allowance. Direct placeholders remain denied
in tests. The test-only macro allowance means local wrappers can conceal
placeholders from Clippy; review is still required. Unused, unexpanded macro
bodies are outside semantic lint coverage. Do not move production algorithms
into test-only scopes to bypass enforcement.

Run Clippy against the actual Cargo targets using the repository configuration:

```sh
cargo clippy --locked --lib --bins --all-features -- -D warnings
cargo clippy --locked --all-targets --all-features -- -D warnings
```

CI proposes normal and all-target checks on Linux/macOS and a native Windows
production check. Configuration is not evidence of successful remote execution.
A host build does not compile every platform-specific branch.

## Included migration and remaining work

This draft preserves the merged test-boundary and shared-state fixes on its
base. It includes fallible build-document generation, provider header/credential
encoding, compaction marker validation, workspace scanning, resilient-filesystem
publication, credential authority, ACP/runtime attachment, MCP lifecycle, and
TUI control-flow migrations. Errors are propagated rather than converted into
defaults or successful continuation. Actor admission and publication retain
ownership of rollback until a complete transition commits.

Production JSON construction uses explicit `Value`/`Map` conversions for concrete
JSON-compatible values and propagates fallible serialization errors. Clap's
builder and fallible argument-access APIs replace derives that emit conflicting
lint allowances. Tokio runtime construction is fallible. These changes avoid
panic-generating expansion paths without exempting macros or weakening any rule.

Async races use existing future/stream combinators or concrete local polling.
Previously biased selections retain their priority. Repeated unbiased loops now
use rotating priority or persistent ready-event merging to preserve progress;
this is not identical randomized tie-breaking. One-shot races document their
now-deterministic, previously permitted simultaneous-ready outcomes. Losing
futures and their owning scopes end before handlers reuse receivers, reset
terminal input, or terminate and reap child processes.

Two policy prerequisites remain: the upstream persistence contract described
below, and agreement on the correctly test-gated `src/tools/subagent/tests.rs`
module. That module lacks the narrow assertion/unwrap allowances already used
by the other test modules, including allowances for their macro expansions.
Its assertions have not been weakened or exempted during this migration.

No dependency revision, version, feature, or production lint configuration is
changed by these callsite migrations. Native platform coverage remains required;
a successful host check does not establish behavior in other conditional code.

### Guarded-state inventory

- Parallel walker callbacks are the only writers of the local file vector and
  first-error slot. They acquire one lock at a time, build strings before locking,
  and do not await or invoke callbacks while holding a guard. A rejected poisoned
  lock stops that walker; after all walkers join, consuming either poisoned
  owner returns an error rather than accepting partial results. No poison
  recovery is introduced.
- `search_workspace` is the cache writer. Its blocking worker serializes refreshes
  with the cache lock; the new snapshot and query must succeed before replacement.
  Failure retains the old activation and snapshot. Replaced data is dropped after
  unlocking. The worker has no await under the guard; cancelling its async caller
  can leave the blocking worker running, as before, but cannot publish a partial
  snapshot. Walker callbacks use only their private scan locks and cannot acquire
  the cache lock in reverse order.
- Failure-path tests cover poisoned scan owners and rejected refreshes using the
  real scan/cache interfaces. This is a bounded inventory, not a repository-wide
  shared-state audit.
- The subscription catalog cache has one entry writer, `get_or_try_init`. It
  prepares a complete binding/OnceCell pair before replacing the entry, releases
  the async mutex before initializing the cell, and drops retired values outside
  the guard. There is no await or callback while the guard is held. Initializer
  failure, unwind, or cancellation leaves a complete retryable cell; an old
  binding's late initializer can only complete its own cell, not replace a newer
  binding. Tests cover retry, cancellation/unwind, and stale initialization.
- Resilient-filesystem submission returns one acceptance/rejection result, rather
  than independently mutable boolean/result states. Rejection precedes logical
  image, cursor, and namespace commits. Replay validates parent paths before
  effects; existing lease fencing and poison isolation remain authoritative.
  Credential mutations acquire real refresh authority before changing storage.
- Session claims cover acquisition, transcript guarding, commit, fork deferral,
  and drop. Rejected deferral retains rollback ownership. Typed attachments carry
  required creation ownership rather than discovering its absence after
  publication. Both ACP versions acquire fallible MCP subscriptions before
  binding or starting drivers, so rejected admission releases the claim.
- MCP event-route admission and subscription drop are the route-map writers.
  Checked generation allocation prevents wraparound and stale-owner reuse.
  Complete routes are published under the lock; replaced senders are dropped
  afterward. Cleanup removes only its own generation. Poison is isolation, not
  permission to recover uncertain route contents.
- OAuth generation, cleanup-pending, and terminal isolation share one atomic
  phase. Admission and completion classify a single snapshot. Reservation
  commits at a successful CAS while holding the token guard through pending-state
  publication; failed CAS classifies contention without fencing healthy owners.
  Cleanup-pending rejects new work but permits exact-generation settlement.
- Refresh reservation, backend completion, resolver publication, normal finish,
  cleanup claim/acknowledgment/drop, reload, and authorization replacement all
  participate in OAuth ownership. Backend leases recheck after acquiring the
  manager and isolate abnormal exits before unlocking it. Ordered registry
  transitions validate identity, generation, and phase at commit, publish
  prepared replacements without intervening awaits, and drop retired values
  after unlocking. The lock order includes challenges, pending authorization,
  OAuth sessions, active replays, server records, and token state; backend
  manager acquisition is outside registry locks and precedes token access.
- Cleanup obligations are constructed before task ownership is relinquished.
  Cleanup remains pending until backend work and registry cleanup settle.
  Exact-generation acknowledgment permits reuse; abandonment, including an
  unpolled task or executor shutdown, terminal-fences that owner. Stale drops,
  acknowledgments, and callbacks cannot fence or publish into a newer generation
  or replacement session. Interrupted authorization workers and reloads retain
  explicit failure state in surviving owners instead of appearing usable.
- Child, shell, and TUI arbitration keeps pending wakeups alive and consumes only
  the selected event. Fairness state persists across repeated decisions. Child
  request/serialization ownership and shell reader/process cleanup outlive losing
  caller futures where required; dropping a join handle is not treated as reaping.

This inventory and its failure-path coverage are bounded, not a proof over all
schedules or dependency-internal rollback. Existing subagent create/fork callbacks
under registry guards remain a separate reentrancy/unwind audit boundary.

## Unresolved persistence prerequisite

The pinned `agentkit-loop` revision
`8e4ee26434a3f847e3613da5bb073ae63a262243` exposes
`TranscriptObserver::on_transcript_event` returning `()`. `append_item` calls it
immediately before an unconditional in-memory transcript push. Logging,
requesting shutdown, cancellation, or an early return still permits that commit.
Checking only an outer driver boundary misses append paths such as final
assistant output, tool dispatch, cancellation cleanup, and approval retirement.

`src/session.rs` therefore retains the current poisoned-writer `expect` and
persistence-failure `panic!` unchanged. The existing storage-exhaustion/shutdown
early return has the same unpersisted-continuation hazard and must not be reused
as a replacement. The panic also bypasses asynchronous actor cleanup: retaining
it is a blocker, not an endorsed solution. No default, silent continuation,
process abort, or production `catch_unwind` replaces it.

The required upstream and host contract is:

1. A fallible synchronous persistence acknowledgment with a distinguishable error.
2. No in-memory commit for a rejected item; batches stop at the committed prefix.
3. Propagation through inputs, assistant output, tools, detach placeholders,
   approvals, cancellation, and interrupted-turn repair.
4. Terminal isolation: no further inference, tool dispatch, success response, or
   reuse of the failed driver; do not misreport the failure as cancellation.
5. Cleanup that cancels/retires owned work without requiring writes to the failed
   sink, while preserving the primary error.
6. Explicit semantics for uncertain writes, external effects already executed,
   multiple-observer ordering, and retry.
7. Host-side terminal reporting and session retirement, not an error followed by
   continued actor-loop operation.

This prerequisite requires separate authorization and upstream work. Until it
and the remaining migration/expansion work are resolved, this PR stays draft.
