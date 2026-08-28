# Whole-Branch Review Fix Report

## Outcome

Both Important findings were valid against the approved process-local/session design and were fixed. There is no reviewer pushback.

- A TUI `/new` or `/resume` replacement selects a different process-local runtime session, so its live Agents roster must begin empty rather than inheriting rows or reducer metadata from the replaced session.
- `SubagentDescendantsRemoved` is intentionally strict-descendant cleanup. It must not synthesize the direct ancestor transition; the owning `Subagents` registry remains responsible for exactly one direct `removed` event and terminal resource release.

## Root causes

### 1. Replacement sessions retained stale TUI roster state

`App::start_session` reset transcript/session-derived fields but omitted the complete roster reducer state: `agents`, `agent_versions`, `cleaned_agent_ids`, scroll offset, viewport row count, and panel hit area. After `runtime_session_id` moved to the replacement session, old-runtime events were correctly gated out, making the stale rows permanent and allowing new-session rows to merge with them.

The fix clears the complete live roster reducer and its viewport metadata in `start_session`. Panel visibility remains a user preference and is intentionally preserved.

### 2. Unexpected idle ACP exits had no direct-registry notification

ACP stderr EOF already emitted strict-descendant cleanup, but `ChildSession` exposed only an operation-time `is_closed` probe. The owning registry therefore learned of process death only during a later reserve/prompt path. An idle direct handle could retain its row, name, and semaphore permit indefinitely.

The fix adds a cloneable child-process closure signal, set as soon as process reaping observes exit and also when the actor terminates. Each direct registry entry installs a weak-state monitor. On closure it atomically retires the matching live state, preserves the completed success outcome for an idle handle (or marks a non-idle generation failed), removes the registry entry, releases capacity/name ownership, and emits one direct event. Idempotent retirement prevents races with prompt failure or explicit close from emitting duplicates. The weak state reference avoids delaying permit release for explicitly closed sessions, including sessions sharing an ACP process. Nested forwarding remains unchanged and emits no synthetic ancestor transition.

## Strict TDD evidence

### RED

- `cargo test --lib tui::app::tests::switching_sessions_clears_the_process_local_agent_roster -- --nocapture`
  - Failed at `assert!(app.agents.is_empty())` after `start_session("replacement")`.
- `cargo test --lib tools::subagent::tests::lifecycle_events::idle_child_exit_promptly_retires_the_direct_handle_once -- --nocapture`
  - The mock ACP completed one successful generation and exited. Strict-descendant cleanup appeared, but the test timed out waiting one second for the direct `removed` event.

The TUI test had one initial scaffolding compile/setup correction (`Rect` import and direct tombstone seeding) before the behavioral RED above; no production code was edited before both behavioral failures were observed.

### GREEN

The same two focused regression commands passed after the production changes. Additional focused compatibility checks passed:

- `cargo test --lib tools::subagent::tests::close_does_not_block_listings_or_allow_stale_reuse -- --nocapture`
- `cargo test --lib tools::subagent::tests::lifecycle_events::closed_sessions_emit_one_removed_transition_before_name_reuse -- --nocapture`

The new idle-exit regression verifies the exact lifecycle sequence, one and only one direct `removed` event, immediate registry removal, full semaphore restoration, and `Scout` name reuse without any intervening prompt/list/create trigger before observing removal.

## Changed files

- `src/tui/app.rs` — clear complete process-local roster state during session replacement; add regression coverage.
- `src/acp_child.rs` — expose actor/process closure notification shared by forked sessions.
- `src/tools/subagent.rs` — monitor unexpected child exits, retire direct entries exactly once, and add lifecycle/resource regression coverage.
- `fixtures/mock-acp.py` — add a deterministic exit-after-success mode used by the regression.
- `Cargo.toml`, `Cargo.lock` — patch bump from `0.1.104` to `0.1.105` per repository policy.
- `.superpowers/sdd/2026-08-28-named-subagent-agents-panel/whole-branch-fix-report.md` — this report.

## Verification and commits

Implementation commit: `fc543ab` (`fix: retire stale subagent roster state`).

Checks run successfully:

- `cargo fmt --check`
- `cargo test --lib tools::subagent::tests -- --nocapture`
- `cargo test --lib acp_child::tests -- --nocapture`
- `cargo test --lib tui::app::tests -- --nocapture`
- `cargo test --quiet`
- `git diff --check`

No subagents were dispatched. Code-review delegation was intentionally omitted to honor the explicit instruction not to dispatch subagents.
