# Faker first-name naming strategy implementation report

## TDD evidence

### RED

Added allocator tests before production changes for first-choice normalization, case-insensitive clash rerolls, multiple clashes, invalid candidates, exact bounded exhaustion, release/reuse, real-faker validation, and atomic concurrent insertion.

Command:

```text
cargo test --lib tools::subagent::tests::faker_allocator -- --nocapture
```

Observed exit 101. Compilation failed because the existing allocator still required an indexed curated-catalog callback and because `faker_first_name` and `normalize_display_name` did not exist. This was the expected feature-missing RED.

### GREEN

Focused commands passed after the minimal implementation:

```text
cargo test --lib tools::subagent::tests::faker_allocator -- --nocapture
cargo test --lib tools::subagent::tests::real_english_faker -- --nocapture
cargo test --lib tools::subagent::tests::manager_name_allocation_is_atomic -- --nocapture
cargo test --lib tools::subagent::tests::exactly_64_failed -- --nocapture
cargo test --lib tools::subagent::tests::fork_allocates_a_fresh_faker_name -- --nocapture
cargo test --lib tools::subagent::tests::name_reservations_are_case_insensitive -- --nocapture
cargo test --lib tools::subagent::tests -- --nocapture
```

The existing prompt/lifecycle and legacy-handle tests remained in the affected module suite. Prompt keeps its allocated name; fork now has deterministic candidate injection proving a fresh allocation. Registry insertion remains under the sessions mutex, so candidate checking and reservation are atomic.

## Dependency decision

- Added `fake = { version = "5.1.0", default-features = false }`, the current crate release inspected with `cargo info` and local crate metadata. English `faker::name::en::FirstName` needs no optional `fake` feature.
- Enabled `fake` features: none (all 32 optional/default features disabled). In particular, the default `either` feature and CLI/derive integrations are disabled.
- `cargo tree -e features -p fake` shows mandatory runtime edges to `deunicode` (`default`, `alloc`) and `rand` (`default`, `chacha`); `rand` uses its existing `getrandom 0.4.3` transitive. Only `fake 5.1.0` and `deunicode 1.6.2` were newly locked because the needed rand/getrandom versions were already present.
- No naming-specific direct `getrandom` dependency or call was added. Kit's pre-existing direct `getrandom = "=0.4.3"` remains for unrelated plugin/provider code.
- `cargo tree --locked -i petname` reports no matching package.
- Package and lock package version remain exactly `0.1.109`.

## Behavior and files

- `src/tools/subagent.rs`: injects a thread-safe candidate generator, uses faker English first names in production, trims and validates 1–32 ASCII alphabetic display names, rerolls invalid/clashing candidates case-insensitively for exactly 64 attempts, then reserves the lowest available `Agent N`. Successful faker names are never suffixed. Existing registry lifecycle controls reservation release.
- `src/tools/agent_names.txt`: removed with all compile-time catalog code.
- `Cargo.toml`, `Cargo.lock`: minimal faker dependency and lock updates.
- `docs/superpowers/specs/2026-08-28-named-subagent-activity-roster-design.md`: updated naming design and independent nested-process scope.
- `docs/superpowers/plans/2026-08-28-named-subagent-agents-panel.md`: updated implementation/testing plan while retaining version `0.1.109`.
- `docs/user/subagents-and-acp-harnesses.md`: updated user behavior, 64-attempt fallback, no suffixes, and possible cross-branch duplicates.

Immutable IDs, event forwarding, legacy optional-name handles, Agents tree behavior, and top-level display paths were unchanged. No AI/model name input or naming configuration was introduced.

## Verification

- `cargo fmt --all --check` — passed.
- `cargo test --locked` — passed, including doc tests.
- `git diff --check` — passed before commits.
- Strict `cargo clippy --locked --all-targets --all-features -- -D warnings` — no new findings. It stops on exactly the five known baseline findings in unchanged files: one `while_let_loop` in `src/tui/markdown.rs:522` and four `unnecessary_fold` findings in `src/tui/plan.rs:123,262,265,289`.
- Stale-claim search across `src/tools/subagent.rs` and the updated design/plan/user docs found no curated-catalog, 350-name, petname, naming-getrandom, include resource, or suffix-allocation claims.

## Commits

- `c630379 feat: generate subagent first names with fake`
- `1b85a11 docs: describe faker subagent names`
