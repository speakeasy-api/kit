# Intentional panic policy

> **Published as a blocked draft — migration is incomplete, not merge-ready.**
> The proposed enforcement intentionally rejects remaining production callsites
> and dependency-generated expansions. These are blockers, not grandfathered
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

The dependency-free fixtures use Rust/Clippy 1.94.0 and the repository config.
They exercise alternate syntax, const calls, local overrides, production code
in test builds, actual root/manifest wiring, test helpers, and known test-only
limitations. Build-script process fixtures exercise invalid/missing inputs,
missing output directory, write rejection, and symlink rejection.

```sh
python3 scripts/tests/test_panic_policy.py
python3 scripts/tests/test_build_docs.py
cargo clippy --locked --lib --bins --all-features -- -D warnings
cargo clippy --locked --all-targets --all-features -- -D warnings
```

CI proposes normal and all-target checks on Linux/macOS and a native Windows
production check. Configuration is not evidence of successful remote execution.
A host build does not compile every platform-specific branch.

## Included migration and remaining work

This draft targets current `main` and preserves its merged test-boundary and
shared-state fixes. It includes fallible build-document generation, provider
header/credential encoding, compaction marker validation, workspace scanning,
and selected TUI control-flow migrations. Errors are propagated rather than
converted into defaults or successful continuation.

The broader CLI builder rewrite and coupled ACP/runtime, MCP lifecycle,
credential-lock, resilient-filesystem publication, and TUI app/dialog migrations
are not included. Their current-main implementations remain authoritative,
including fallible actor-token allocation and early v2 busy admission. Completing
and independently reviewing these callsites is a prerequisite to enforcement,
not a reason to add production allowances. CLI Clap derives also emit lint
allowances that conflict with production `forbid`; replacing or accommodating
those expansions needs a compatibility-preserving design and review.

The stronger disallowed rules also diagnose generated code from external
`serde_json::json!`, `tokio::select!`, and `tokio::join!` expansions. Source-level
migrations do not solve that limitation. A precise enforcement strategy must
retain direct-call, UFCS/function-item, and local-wrapper coverage without
blanket macro exemptions. No dependency upgrade, vendoring, or patch is included.
The binary and platform-specific policy inventories are not complete behind
these blockers. Prior worktree diagnostic/test counts are not a current result
for this draft.

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
