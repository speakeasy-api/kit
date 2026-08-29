# Petname naming revision implementation report

## Summary

Replaced configurable/model-supplied subagent names with internally generated one-word petnames. New subagents and forks allocate names atomically; prompts preserve names; close, failed creation, and terminal retirement continue to release registry reservations. Legacy serialized handles without `name` remain readable. The package version remains `0.1.104`.

## TDD evidence

### RED

Tests were changed before production code.

- `cargo test --locked tools::subagent::tests::petname_candidates_are_one_word_capitalized_and_collision_safe` failed to compile because `allocate_name_with` and `MAX_PETNAME_ATTEMPTS` did not exist. This was the expected missing-allocator failure.
- `cargo test --locked --bin kit subagent_names_are_rejected_as_unknown_configuration` failed because `Config::load` still accepted `names = ["Scout"]` and populated `SubagentConfig.names`.
- The first full `cargo test --locked` exposed an existing forward-compatibility expectation: a blanket `deny_unknown_fields` rejected unrelated future `[subagent]` options. Strict rejection was narrowed to the removed `names` key, preserving that compatibility test.

### GREEN

- Focused petname allocator tests: 2 passed.
- Removed naming-input schema test: passed.
- Removed config test: passed.
- Fresh fork-name test: passed.
- Full subagent module: 34 passed before the explicit fork test was added; the fork test then passed independently.
- Final `cargo test --locked`: library 564 passed / 2 ignored; binary 20 passed; integration 16 passed; no failures.

## Dependency and features

Added `petname = "=3.2.0"` with `default-features = false` and only `default-rng` plus `default-words`. The `clap` feature is not enabled. `default-words` brings the crate's required `petname-macros` support. `Cargo.lock` records `petname` and `petname-macros`; the root package remains version `0.1.104`.

## Behavior and compatibility

- Up to 32 generated candidates are attempted.
- Candidates must normalize to one non-empty, control-free, whitespace-free word of at most 32 Unicode scalar values and are consistently capitalized.
- Live sibling uniqueness remains case-insensitive. Collisions use the lowest available `Name 2`, `Name 3`, and so on, truncating the base to preserve the 32-character display bound.
- `Agent N` is used only when bounded petname generation cannot provide an available valid candidate.
- `subagent` and `fork` inputs contain no naming field; tool schemas and descriptions reflect internal naming.
- `[subagent].names` is no longer typed or applied and is explicitly rejected, while unrelated future `[subagent]` options remain forward-compatible.
- `SubagentValue.name` remains optional on deserialization for legacy handles and populated for current handles.

## Files changed

- `Cargo.toml`, `Cargo.lock`
- `src/tools/subagent.rs`, `src/tools/mod.rs`
- `src/main.rs`
- `src/runtime.rs`, `src/runtime/tests.rs`
- `docs/user/subagents-and-acp-harnesses.md`
- `docs/superpowers/specs/2026-08-28-named-subagent-activity-roster-design.md`
- `docs/superpowers/plans/2026-08-28-named-subagent-agents-panel.md`
- this report

## Checks

- `cargo fmt --all --check` — passed
- targeted allocator/config/schema/fork tests — passed
- `cargo test --locked tools::subagent::tests` — passed
- `cargo test --locked` — passed
- `git diff --check` — passed
- version checks for `Cargo.toml` and root `Cargo.lock` package — both `0.1.104`
