# Named Subagent Agents Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every Kit subagent a stable friendly name and show all observable foreground, background, and nested descendants in a dedicated always-expanded tree TUI Agents panel.

**Architecture:** Keep direct-child truth in `Subagents.sessions`, emit structured process-safe lifecycle events, and recursively forward child Kit events to a top-level TUI reducer keyed by immutable agent ID. Render that reducer in an independently toggled 46-column panel without changing the existing graph.

**Tech Stack:** Rust, Tokio, Serde/JSON, Ratatui, Crossterm, TOML configuration, ACP child-process transport.

**Spec:** `docs/superpowers/specs/2026-08-28-named-subagent-activity-roster-design.md`

## Global Constraints

- Invoke `updating-artifact-schema` before changing `[subagent]` config or serialized `SubagentValue` handles.
- Reject removed name configuration while preserving old serialized handles that omit `name`; current outputs always populate `name`.
- Keep immutable `s-…` IDs authoritative; display names never select handles.
- Generate English first names with `fake` using no default features; model inputs never provide names.
- Name uniqueness is case-insensitive only among one parent's direct children.
- Runtime event timestamps are Unix epoch milliseconds.
- Event delivery remains observational and cannot fail subagent work.
- `Ctrl+G` and the existing graph remain unchanged; `Ctrl+R` and `/agents` toggle the new panel.
- Wide three-panel layout begins at 154 columns; narrow three-panel layout is transcript/graph/Agents at 40/30/30 percent.
- Keep the package version exactly `0.1.109` for this unreleased revision, including after dependency lock changes.

---

## File Structure

- `src/tools/subagent.rs`: random name allocation/reservation, lifecycle state, handle/listing schemas, lifecycle event emission.
- `src/runtime.rs`: reconstruct subagent managers without naming policy.
- `src/main.rs`: strictly reject removed subagent naming configuration.
- `src/events.rs`: shared subagent status/outcome types and lifecycle/descendant-removal wire events.
- `src/acp_child.rs`: pass parent identity into child Kit processes and forward nested roster events/cleanup.
- `src/tui/app.rs`: reduce roster events, hold sorting/counting/scroll state, and handle `Ctrl+R`.
- `src/tui/command.rs`: parse and highlight `/agents`.
- `src/tui/ui.rs`: render the two-line always-expanded tree panel, footer, spinner palette, and responsive layouts.
- `docs/user/subagents-and-acp-harnesses.md`: document random naming, emergency fallback, and observable descendants.
- `docs/user/tui-and-sessions.md`: document the Agents panel, controls, states, and layout.
- `Cargo.toml`, `Cargo.lock`: add the minimal faker dependency while keeping the unreleased package version at `0.1.109`.

---

### Tasks 1–2: Random Names, Handles, Listings, and Lifecycle State

**Files:** `Cargo.toml`, `Cargo.lock`, `src/tools/subagent.rs`, `src/runtime.rs`, `src/main.rs`, and focused tests.

- Inventory all name configuration, allocator, request-schema, serialized-handle, and lifecycle readers/writers before editing.
- Add RED tests for injected candidates covering first-choice allocation, normalization, invalid-candidate rerolls, case-insensitive single and multiple clashes, exactly 64 failed attempts before the lowest `Agent N` fallback, release/reuse, and atomic concurrent insertion. Keep a small real-faker smoke test that validates shape without fixing a random value.
- Add the Rust `fake` crate with default features disabled and generate English `FirstName` values. Trim and accept only safe 1–32 character ASCII alphabetic display names; reroll invalid values and clashes rather than adding numeric suffixes.
- Allocate atomically while inserting the starting registry entry. Preserve names on `prompt`, perform a fresh allocation on `fork`, and release reservations on failed creation, close, or terminal retirement. Nested Kit processes remain independent, so duplicates across descendant branches are possible.
- Keep `SubagentValue.name` optional when reading legacy handles while populating it in current outputs. Keep immutable IDs authoritative and retain the strict listing shape.
- Run focused allocator/config/schema tests, then the full subagent module before proceeding.

### Task 3: Structured Runtime Lifecycle Events

**Files:**
- Modify: `src/events.rs:36-230`
- Modify: `src/tools/subagent.rs:140-486`

**Interfaces:**
- Consumes: lifecycle fields from Task 2
- Produces: `events::SubagentStatus` and `events::GenerationOutcome`
- Produces: `RuntimeEvent::SubagentStateChanged` and `RuntimeEvent::SubagentDescendantsRemoved`
- Produces: `RuntimeEvent::parent_call() == None` for roster events

- [ ] **Step 1: Write failing event round-trip tests**

Construct both new event variants, serialize with `emit` framing expectations, parse them back, and assert snake-case status/outcome names and Unix-ms fields. Add a test that `parent_call()` returns `None`.

```rust
let event = RuntimeEvent::SubagentDescendantsRemoved { ancestor_id: "s-parent".into() };
assert_eq!(parse(&format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap())), Some(event));
```

- [ ] **Step 2: Run event tests and confirm failure**

Run: `cargo test --lib events::tests -- --nocapture`

Expected: FAIL because the new types and variants do not exist.

- [ ] **Step 3: Implement wire types and variants**

Derive `Clone`, `Debug`, `Serialize`, `Deserialize`, `PartialEq`, and `Eq`; use `#[serde(rename_all = "snake_case")]`. Keep existing marker framing unchanged.

- [ ] **Step 4: Write failing lifecycle emission tests**

With runtime events enabled in the test process, capture emitted events for initial create, success to idle, reusable failure to idle with failed outcome, close to removed, failed creation to removed, and terminal retirement to removed. Verify state mutation happens before emission and that event-write failure cannot alter the tool result.

- [ ] **Step 5: Run emission tests and confirm failure**

Run: `cargo test --lib tools::subagent::tests::lifecycle_events -- --nocapture`

Expected: FAIL because transitions do not emit structured events.

- [ ] **Step 6: Emit events after committed state transitions**

Centralize event construction on `State` so every operation reports the same authoritative fields. Emit `starting`, `working`, `idle`, and `removed` only after the registry transition succeeds. Treat `events::emit` as best effort.

- [ ] **Step 7: Run event and subagent tests**

Run: `cargo fmt --check && cargo test --lib events::tests -- --nocapture && cargo test --lib tools::subagent::tests::lifecycle_events -- --nocapture`

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/events.rs src/tools/subagent.rs
git commit -m "feat: emit subagent lifecycle events"
```

---

### Task 4: Nested Parent Context and Recursive Cleanup

**Files:**
- Modify: `src/acp_child.rs:52-70, 343-355, 487-562, 728-905`
- Modify: `src/main.rs:455-476, 858-912`
- Modify: `src/events.rs`
- Test: existing inline tests in `src/acp_child.rs` and `src/main.rs`

**Interfaces:**
- Consumes: lifecycle events from Task 3
- Produces: optional internal parent identity in `ChildConfig`/child CLI startup
- Produces: nested event forwarding without rewriting child IDs or timestamps
- Produces: `SubagentDescendantsRemoved { ancestor_id }` when a child process or event channel ends

- [ ] **Step 1: Write failing child-context argument tests**

Extend child command assertions so a Kit child launched for named handle `Scout` receives internal parent ID and name values, while generic ACP harness commands do not require understanding them. Cover Unicode names and absence for the top-level runtime.

- [ ] **Step 2: Run context tests and confirm failure**

Run: `cargo test --lib acp_child::tests::subagent_parent_context -- --nocapture`

Expected: FAIL because no parent context is propagated.

- [ ] **Step 3: Implement internal parent context**

Add hidden ACP CLI arguments or private environment fields for parent subagent ID/name. Feed them into the child runtime's event producer context; do not expose them as user configuration. Ensure recursive launches replace the immediate-parent context rather than appending an unbounded ancestry payload.

- [ ] **Step 4: Write failing forwarding and cleanup tests**

Feed marker-prefixed `SubagentStateChanged` lines through the child stderr parser and assert exact forwarding. Simulate normal EOF and abrupt child exit and assert one `SubagentDescendantsRemoved { ancestor_id }`. Feed duplicate cleanup/state events into a reducer fixture and assert idempotence.

- [ ] **Step 5: Run forwarding tests and confirm failure**

Run: `cargo test --lib acp_child::tests::forwards_subagent_events -- --nocapture && cargo test --lib acp_child::tests::removes_descendants_on_exit -- --nocapture`

Expected: FAIL because only existing child start/finish events are forwarded.

- [ ] **Step 6: Implement recursive forwarding and strict-descendant cleanup**

Extend the current event filter in `acp_child.rs` to forward roster variants. Emit descendant cleanup whenever the owning child process or forwarding stream ends. The ancestor's own direct registry transition remains responsible for its row, outcome, and removal.

- [ ] **Step 7: Run child and event tests**

Run: `cargo fmt --check && cargo test --lib acp_child::tests -- --nocapture && cargo test --lib events::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/acp_child.rs src/main.rs src/events.rs
git commit -m "feat: forward nested subagent rosters"
```

---

### Task 5: TUI Roster Reducer, Commands, and Input

**Files:**
- Modify: `src/tui/app.rs:480-1140, 1938-2130`
- Modify: `src/tui/command.rs`
- Modify: `src/tui/mod.rs:320-375`

**Interfaces:**
- Consumes: `RuntimeEvent::SubagentStateChanged` and `SubagentDescendantsRemoved`
- Produces: `AgentRow` state keyed by immutable ID
- Produces: `App::show_agents()`, `App::toggle_agents()`, sorted rows, counts, and independent scroll offset
- Produces: `Parsed::Agents` and `Ctrl+R` behavior

- [ ] **Step 1: Write failing reducer tests**

Test direct and nested insertion, duplicate visible labels with distinct IDs, updates across generations, out-of-order duplicate events, strict-descendant cleanup, immediate footer exclusion for removed tombstones, four-second failed-row grace, and deterministic starting/working/idle ordering. Inject Unix-ms `now` into reducer helpers rather than sleeping.

```rust
app.apply_runtime(RuntimeEvent::SubagentDescendantsRemoved { ancestor_id: "s-scout".into() });
assert!(app.agents().iter().all(|row| row.parent_chain_excludes("s-scout")));
```

- [ ] **Step 2: Run reducer tests and confirm failure**

Run: `cargo test --lib tui::app::tests::agents -- --nocapture`

Expected: FAIL because App has no global roster state.

- [ ] **Step 3: Implement the reducer and independent state**

Store rows in `HashMap<String, AgentRow>`, retain failed removed tombstones separately or mark them non-live, and prune four-second grace entries from `App::tick`. Calculate footer counts from live statuses only. Clamp roster scroll after every state/layout change.

- [ ] **Step 4: Write failing command and keyboard tests**

Assert `/agents` parses only as an exact local token, is recognized by highlighting, toggles without sending a prompt, and `Ctrl+R` toggles the same state. Assert `Ctrl+A` still moves the editor cursor and `Ctrl+G` still only controls graph visibility.

- [ ] **Step 5: Run command/input tests and confirm failure**

Run: `cargo test --lib tui::command::tests -- --nocapture && cargo test --lib tui::app::tests::agents_toggle -- --nocapture`

Expected: FAIL because `/agents`, `Parsed::Agents`, and the key binding do not exist.

- [ ] **Step 6: Implement command and input behavior**

Add `Kind::Agents`/`Parsed::Agents`, register `/agents`, handle it beside other local commands, and add `KeyCode::Char('r') if control` after editor movement bindings. Do not alter `Ctrl+A` or `Ctrl+G`.

- [ ] **Step 7: Route mouse scrolling by panel bounds**

Record the last rendered Agents rectangle in App. When a wheel event falls inside it, update only roster offset and consume the event; otherwise preserve transcript/graph behavior. Add boundary and saturation tests.

- [ ] **Step 8: Run TUI state tests**

Run: `cargo fmt --check && cargo test --lib tui::command::tests -- --nocapture && cargo test --lib tui::app::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/tui/app.rs src/tui/command.rs src/tui/mod.rs
git commit -m "feat: track agents in the TUI"
```

---

### Task 6: Agents Panel Rendering and Responsive Layout

**Files:**
- Modify: `src/tui/ui.rs:38-48, 418-434, 1020-1120, test module`
- Reuse: `src/tui/theme.rs:49-105, 165-180`

**Interfaces:**
- Consumes: sorted roster rows/counts/scroll offset from Task 5
- Produces: `draw_agents(frame, app, area)`
- Produces: two-line row formatter with reserved right-aligned duration column
- Produces: exact transcript/graph/Agents responsive rectangles

- [ ] **Step 1: Write failing row-format tests**

Use Ratatui `TestBackend` or existing line helpers to assert:

```text
○ Pip
│  └─ ⠋ Scout
│     Trace ACP lifecycle                     1m 12s
```

Cover top-level omission of `via`, nested ancestry, same visible labels, task truncation before the timer, narrow inner widths, starting `Pulse::Child` yellow, working `Pulse::Tool` cyan, idle dim `○`, failed red `✗`, and Unicode-safe truncation.

- [ ] **Step 2: Run formatter tests and confirm failure**

Run: `cargo test --lib tui::ui::tests::agent_rows -- --nocapture`

Expected: FAIL because the Agents renderer does not exist.

- [ ] **Step 3: Implement panel and footer rendering**

Draw a bordered `AGENTS` panel, reserve two lines per visible row plus one fixed footer line, and render only nonzero footer buckets in `total · starting · working · idle` order. Use `theme::pulse(Pulse::Child, app.tick)` for starting and `Pulse::Tool` for working. Use `theme::duration` and right-align it inside the second line.

- [ ] **Step 4: Write failing responsive layout tests**

Assert exact rectangles at representative widths:

- 107 columns, one side panel: vertical 55/45.
- 108 columns, one side panel: transcript + 46-column side panel.
- 153 columns, both panels: vertical transcript/graph/Agents at 40/30/30.
- 154 columns, both panels: transcript remainder + 46-column graph + 46-column Agents.
- Graph hidden/Agents shown and Graph shown/Agents hidden each retain the one-panel behavior.

- [ ] **Step 5: Run layout tests and confirm failure**

Run: `cargo test --lib tui::ui::tests::agents_layout -- --nocapture`

Expected: FAIL because `draw_body` only handles graph visibility.

- [ ] **Step 6: Implement all four visibility combinations**

Refactor `draw_body` around `(app.show_graph(), app.show_agents())`. Keep `SIDE_BY_SIDE_WIDTH = 108`, `GRAPH_WIDTH = 46`, add `AGENTS_WIDTH = 46` and `THREE_COLUMN_WIDTH = 154`, and use the exact ordering and percentages from the spec. Record Agents bounds for mouse routing.

- [ ] **Step 7: Run TUI rendering tests**

Run: `cargo fmt --check && cargo test --lib tui::ui::tests -- --nocapture && cargo test --lib tui::app::tests::agents -- --nocapture`

Expected: PASS with existing graph snapshots/assertions unchanged except deliberate body-layout cases.

- [ ] **Step 8: Commit**

```bash
git add src/tui/ui.rs
git commit -m "feat: render the subagent agents panel"
```

---

### Task 7: Documentation, Version, and Final Verification

**Files:**
- Modify: `docs/user/subagents-and-acp-harnesses.md`
- Modify: `docs/user/tui-and-sessions.md`
- Modify: `Cargo.toml:3`
- Modify: `Cargo.lock` root package entry

**Interfaces:**
- Consumes: all completed behavior
- Produces: user-facing configuration/control documentation while retaining package version `0.1.109`

- [ ] **Step 1: Update user documentation**

Document random one-word naming, bounded emergency fallback, reservation-until-close, sibling-only uniqueness, direct-child `subagents({})` listing, observable nested Kit descendants, generic-harness limitation, foreground/background inclusion, row glyphs, footer, `Ctrl+R`, `/agents`, and responsive panel layout.

- [ ] **Step 2: Add documentation assertions where the repository already tests help/config text**

Extend existing command/config documentation tests to assert `/agents` and `Ctrl+R` remain discoverable and removed naming configuration is absent. Avoid introducing a separate documentation harness.

- [ ] **Step 3: Verify the package version**

Keep the root package and lockfile package version exactly `0.1.109`; dependency lock changes must not alter it.

- [ ] **Step 4: Run formatting and targeted verification**

Run:

```bash
cargo fmt --check
cargo test --bin kit subagent_names_are_rejected_as_unknown_configuration -- --nocapture
cargo test --lib events::tests -- --nocapture
cargo test --lib tools::subagent::tests -- --nocapture
cargo test --lib acp_child::tests -- --nocapture
cargo test --lib tui::command::tests -- --nocapture
cargo test --lib tui::app::tests -- --nocapture
cargo test --lib tui::ui::tests -- --nocapture
```

Expected: all commands PASS.

- [ ] **Step 5: Run repository-wide verification**

Run:

```bash
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Expected: PASS with no warnings. If a pre-existing unrelated failure occurs, preserve its exact command/output and separately rerun all targeted checks above.

- [ ] **Step 6: Inspect the final diff and artifact compatibility**

Run:

```bash
git diff --check
git status --short
git diff --stat
```

Confirm no generated artifacts, companion files, credentials, or unrelated changes are present. Re-run the legacy-handle and absent-config tests as the final persistent-schema compatibility proof.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock docs/user/subagents-and-acp-harnesses.md docs/user/tui-and-sessions.md
git commit -m "docs: document the subagent agents panel"
```

- [ ] **Step 8: Request code review**

Invoke `requesting-code-review` against the complete implementation diff, address blocking findings with focused tests, then invoke `verification-before-completion` before reporting success.
