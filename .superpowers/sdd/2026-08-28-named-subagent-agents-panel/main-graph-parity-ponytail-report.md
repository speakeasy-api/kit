# Main Graph Parity and Ponytail Report

## Root cause

`origin/main` at `4bad70e` removed the TUI runtime graph. Conflict resolution on the Agents-panel branch reintroduced the older `graph_pinned` state, automatic running-tool popup, `Ctrl+G` toggle, graph body-layout variants, renderer/helpers, tests, and documentation. That made active compose/tool calls shrink the transcript and diverged from current main independently of the Agents feature.

The fix removes the graph at its state, input, layout, rendering, test, and documentation boundaries rather than merely hiding it. Version remains `0.1.109`, and the fake-name allocator still performs exactly 64 random attempts before fallback. Superpowers artifacts were not deleted.

## Current-main parity evidence

- Removed `App::graph_pinned`, `App::show_graph`, and the `Ctrl+G` handler.
- Removed graph constants, graph/three-panel `BodyLayout` variants, `draw_graph`, `graph_lines`, `node_style`, graph-specific tests, and graph-specific test setup.
- `draw_body` now computes `(transcript: Rect, agents: Option<Rect>)`, draws the transcript exactly once, then optionally draws Agents.
- Hidden Agents returns the unchanged full body rectangle to the transcript.
- Visible Agents uses only the approved boundary: widths through 107 stack 55/45; widths from 108 place a fixed 46-column Agents panel beside the transcript. Obsolete 153/154 cases are gone.
- The diff against `origin/main` has no changes in transcript/tool-line rendering functions. UI differences are limited to Agents imports, body allocation, Agents rendering, and Agents/regression tests. The branch-restored graph did not leave alternate tool-line parameters or behavior behind.
- Source and user/design/plan searches find no `graph_pinned`, `show_graph`, `draw_graph`, `graph_lines`, `BodyLayout`, `GRAPH_WIDTH`, `THREE_COLUMN`, runtime-graph, or `Ctrl+G` claims. The unrelated terminal graphics-support documentation remains.

## Strict TDD evidence

Tests were added through existing render and key APIs before production changes:

1. `active_compose_keeps_the_full_transcript_layout`
   - RED: active compose rendered `app.transcript_width == 71` in a 120-column terminal because the graph opened automatically.
   - GREEN: the active compose retains the full transcript layout.
2. `ctrl_g_does_not_toggle_a_runtime_graph_layout`
   - RED: pressing `Ctrl+G` rendered `app.transcript_width == 71` in a 120-column terminal.
   - GREEN: `Ctrl+G` does not create or toggle a graph layout.
3. `agents_layout_obeys_hidden_and_107_108_boundaries`
   - GREEN: hidden, width-107 stacked, and width-108 side-by-side rectangles match the approved layout exactly.

Targeted GREEN runs:

- `cargo test --locked --lib tui::app::tests -- --nocapture`
- `cargo test --locked --lib tui::ui::tests -- --nocapture`
- `cargo test --locked --lib tools::subagent::tests -- --nocapture` — 38 passed

## Ponytail reductions

Implementation commit `0ffeb8c` changed 64 lines and deleted 303 lines: **239 net code lines removed**. Documentation commit `07aed5a` changed 25 lines and deleted 30 lines: **5 net documentation lines removed**. Before this report, the complete change was **244 net lines removed**.

Validated cuts applied:

- Deleted the one-line `allocate_name` forwarding wrapper; the sole production allocation site calls `allocate_name_with` directly.
- Deleted the `fork_schema` alias; `ForkTool` passes `continuation_schema()` directly. Schema coverage now inspects the actual `ForkTool` `ToolSpec.input_schema`, not the deleted alias.

## Feedback validation conclusions

- **EventSink retained.** It is the process/test injection boundary for observational, fallible lifecycle transport. `failing_event_transport_does_not_change_subagent_operations` depends on deterministic sink injection; deleting it would either weaken isolation or spread the callable type through production code.
- **FailedRemoval retained.** It is test-only committed-state evidence captured before registry removal, allowing exact assertions for failed cleanup transition status, outcome, and finish timestamp after the session entry no longer exists. Removing it would weaken exact cleanup assertions.
- **Cycle defense retained.** Forwarded/private events may be malformed. `malformed_agent_parent_cycles_render_every_row_once_as_roots` requires cycle detection so every row appears exactly once without recursion loss or duplication. This is approved event-safety behavior, not speculative complexity.
- **Deterministic helpers retained where required.** Name-generator, event-sink, failed-removal, and runtime-event test hooks preserve deterministic collision, transport-failure, and exact cleanup coverage.

## Final checks

- `cargo fmt --all --check` — PASS
- `cargo test --locked` — PASS, including 16 integration runtime tests and doc tests
- `git diff --check` — PASS
- `cargo clippy --locked --all-targets --all-features -- -D warnings` — no new findings; it stops on exactly five known baseline findings in files unchanged from `origin/main`:
  - `src/tui/markdown.rs:522`: one `while_let_loop`
  - `src/tui/plan.rs:123,262,265,289`: four `unnecessary_fold`
- `git diff --quiet origin/main -- src/tui/markdown.rs src/tui/plan.rs` — PASS
- Package version — `0.1.109`
- Fake allocator attempt limit — `MAX_RANDOM_NAME_ATTEMPTS = 64` with passing exact-64 fallback coverage

## Commits

- `0ffeb8c fix: restore current-main tui graph behavior`
- `07aed5a docs: align agents panel with current-main layout`
