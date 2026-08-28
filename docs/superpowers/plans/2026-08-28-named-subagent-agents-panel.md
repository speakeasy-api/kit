# Named Subagent Agents Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every Kit subagent a stable friendly name and show all observable foreground, background, and nested descendants in a dedicated flat TUI Agents panel.

**Architecture:** Keep direct-child truth in `Subagents.sessions`, emit structured process-safe lifecycle events, and recursively forward child Kit events to a top-level TUI reducer keyed by immutable agent ID. Render that reducer in an independently toggled 46-column panel without changing the existing graph.

**Tech Stack:** Rust, Tokio, Serde/JSON, Ratatui, Crossterm, TOML configuration, ACP child-process transport.

**Spec:** `docs/superpowers/specs/2026-08-28-named-subagent-activity-roster-design.md`

## Global Constraints

- Invoke `updating-artifact-schema` before changing `[subagent]` config or serialized `SubagentValue` handles.
- Preserve old configs and old handles that omit `name`; current outputs always populate `name`.
- Keep immutable `s-…` IDs authoritative; display names never select handles.
- Default names are exactly `Scout`, `Pip`, `Juniper`, `Miso`, `Clover`, `Pixel`, `Pebble`, `Nova`, in that order.
- Configured names replace defaults; an explicitly empty list is valid.
- Name uniqueness is case-insensitive only among one parent's direct children.
- Runtime event timestamps are Unix epoch milliseconds.
- Event delivery remains observational and cannot fail subagent work.
- `Ctrl+G` and the existing graph remain unchanged; `Ctrl+R` and `/agents` toggle the new panel.
- Wide three-panel layout begins at 154 columns; narrow three-panel layout is transcript/graph/Agents at 40/30/30 percent.
- Meaningful behavior changes require the package patch version to move from `0.1.101` to `0.1.102`.

---

## File Structure

- `src/tools/subagent.rs`: validated name pools, allocation/reservation, lifecycle state, handle/listing schemas, lifecycle event emission.
- `src/runtime.rs`: carry name-pool policy through every runtime reconstruction path.
- `src/main.rs`: parse and validate `[subagent].names`, then apply it to Serve and ACP runtimes.
- `src/events.rs`: shared subagent status/outcome types and lifecycle/descendant-removal wire events.
- `src/acp_child.rs`: pass parent identity into child Kit processes and forward nested roster events/cleanup.
- `src/tui/app.rs`: reduce roster events, hold sorting/counting/scroll state, and handle `Ctrl+R`.
- `src/tui/command.rs`: parse and highlight `/agents`.
- `src/tui/ui.rs`: render the two-line flat panel, footer, spinner palette, and responsive layouts.
- `docs/user/subagents-and-acp-harnesses.md`: document naming, fallback assignment, and observable descendants.
- `docs/user/tui-and-sessions.md`: document the Agents panel, controls, states, and layout.
- `Cargo.toml`, `Cargo.lock`: patch version bump.

---

### Task 1: Validated Name Pool and Runtime Configuration

**Files:**
- Modify: `src/tools/subagent.rs:1-140`
- Modify: `src/runtime.rs:230-565`
- Modify: `src/runtime/tests.rs`
- Modify: `src/main.rs:187-258, 337-353, 776-912`

**Interfaces:**
- Produces: `SubagentNames::resolve(Option<Vec<String>>) -> Result<SubagentNames, String>`
- Produces: `SubagentNames::as_slice(&self) -> &[String]`
- Produces: `Runtime::with_subagent_names(Arc<Runtime>, SubagentNames) -> Result<Arc<Runtime>, String>`
- Produces: a cloned `SubagentNames` policy in `Subagents::fresh()`

- [ ] **Step 1: Load the persistent-artifact workflow and inventory readers/writers**

Invoke `updating-artifact-schema`. Record in the task notes that `[subagent].names` is read in `Config::load`, while `SubagentValue` is serialized into tool results/transcripts and read by continuation schemas. Confirm the compatibility strategy: optional config field, optional handle input field, populated current output field.

- [ ] **Step 2: Write failing name-pool validation tests**

Add unit tests covering absent defaults, explicit replacement, `Some(vec![])`, trimming, empty values, controls/newlines, more than 32 `char`s, and case-insensitive duplicates. Use assertions of this shape:

```rust
#[test]
fn configured_subagent_names_replace_defaults_and_reject_duplicates() {
    let names = SubagentNames::resolve(Some(vec!["  Acorn  ".into(), "Moss".into()]))
        .expect("valid names");
    assert_eq!(names.as_slice(), &["Acorn", "Moss"]);

    let error = SubagentNames::resolve(Some(vec!["Scout".into(), "scout".into()]))
        .expect_err("case-insensitive duplicate must fail");
    assert!(error.contains("scout"));
}
```

- [ ] **Step 3: Run the focused tests and confirm failure**

Run: `cargo test --lib tools::subagent::tests::configured_subagent_names -- --nocapture`

Expected: FAIL because `SubagentNames` and its resolver do not exist.

- [ ] **Step 4: Implement `SubagentNames` and defaults**

Use an owned, cloneable list and a `HashSet<String>` validation key based on normalized lowercase text. Count `value.chars()`, reject `character.is_control()`, trim only surrounding whitespace for configured values, and preserve source order. Keep absence distinct from an explicit empty vector.

```rust
pub const DEFAULT_SUBAGENT_NAMES: &[&str] =
    &["Scout", "Pip", "Juniper", "Miso", "Clover", "Pixel", "Pebble", "Nova"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentNames {
    names: Arc<[String]>,
}
```

- [ ] **Step 5: Write failing config/runtime propagation tests**

Add `names: Option<Vec<String>>` to the expected `SubagentConfig` shape in tests, then test that absent/custom/empty values survive `Config::load`, `Runtime::with_telemetry`, `with_acp_harnesses`, `with_mcp_config`, and `Subagents::fresh()`. Assert the manager's test-only name slice after each reconstruction.

- [ ] **Step 6: Run config/runtime tests and confirm failure**

Run: `cargo test --bin kit subagent_names -- --nocapture && cargo test --lib runtime::tests::subagent_names -- --nocapture`

Expected: FAIL because config and runtime do not carry the policy.

- [ ] **Step 7: Plumb the validated policy through runtime construction**

Add `SubagentConfig.names: Option<Vec<String>>`, validate during `Config::load`, and call `Runtime::with_subagent_names` in both Serve and ACP setup before tool registries are used. Ensure every runtime reconstruction copies the current policy instead of resetting to defaults.

- [ ] **Step 8: Run focused tests and format**

Run: `cargo fmt --check && cargo test --bin kit subagent_names -- --nocapture && cargo test --lib configured_subagent_names -- --nocapture && cargo test --lib runtime::tests::subagent_names -- --nocapture`

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/main.rs src/runtime.rs src/runtime/tests.rs src/tools/subagent.rs
git commit -m "feat: configure subagent name pools"
```

---

### Task 2: Name Allocation, Handles, Listings, and Lifecycle State

**Files:**
- Modify: `src/tools/subagent.rs:22-486, 540-760, 898-end`

**Interfaces:**
- Consumes: `SubagentNames` from Task 1
- Produces: `State.name`, explicit `SubagentStatus`, task summary, outcome, and Unix-ms fields
- Produces: `allocate_name(pool, sessions, fallback_name) -> String`
- Produces: optional `fallback_name` inputs for `subagent` and `fork`
- Produces: current `SubagentValue.name: Option<String>` compatibility field populated in new outputs
- Produces: structured direct-child `SubagentListing`

- [ ] **Step 1: Write failing allocator and summary tests**

Cover deterministic pool order, reservation until close, released-name reuse, explicit empty pool, fallback normalization, invalid fallback to `Agent N`, suffix collision, 32-`char` truncation, and 96-`char` task summaries. Include concurrent allocation that proves sibling names cannot collide.

```rust
#[test]
fn fallback_is_used_only_after_the_pool_is_exhausted() {
    let mut used = HashSet::new();
    assert_eq!(allocate_name(&["Scout".into()], &mut used, Some("Waffles")), "Scout");
    assert_eq!(allocate_name(&["Scout".into()], &mut used, Some("Waffles")), "Waffles");
}
```

- [ ] **Step 2: Run allocator tests and confirm failure**

Run: `cargo test --lib tools::subagent::tests::name_allocation -- --nocapture`

Expected: FAIL because allocation helpers and reservations do not exist.

- [ ] **Step 3: Implement atomic allocation and lifecycle fields**

Reserve the name while holding the same registry synchronization used for insertion. Insert a `starting` state before asynchronous child startup, then transition it to `working` and `idle`. Release the reservation on failed creation, explicit close, or terminal retirement. Store creation/generation timestamps from `events::now_millis()`.

- [ ] **Step 4: Write failing handle/schema/listing tests**

Test all of the following:

```rust
let legacy = serde_json::from_value::<SubagentValue>(json!({
    "id": "s-old", "output": null, "generation": 1
})).expect("legacy handle remains readable");
assert_eq!(legacy.name, None);
```

Also assert that new outputs populate `name`, `subagent` and `fork` accept `fallback_name`, `prompt` does not, listing rows always contain `id/name/status/generation/task`, and closed/retired entries are absent.

- [ ] **Step 5: Run schema/listing tests and confirm failure**

Run: `cargo test --lib tools::subagent::tests::subagent_value -- --nocapture && cargo test --lib tools::subagent::tests::listing -- --nocapture && cargo test --lib tools::subagent::tests::fallback_name -- --nocapture`

Expected: FAIL on missing fields and old listing shape.

- [ ] **Step 6: Implement tool schemas and compatibility behavior**

Add `fallback_name: Option<String>` to initial and fork request structs. Keep `name` optional when deserializing a handle and in continuation input schemas, ignore supplied handle names for lookup, and always rebuild output handles from authoritative registry state. Replace the untagged starting/ready listing with a single roster row shape.

- [ ] **Step 7: Run the full subagent test module**

Run: `cargo fmt --check && cargo test --lib tools::subagent::tests -- --nocapture`

Expected: PASS, including existing capacity, generation, prompt, fork, close, and cancellation tests.

- [ ] **Step 8: Commit**

```bash
git add src/tools/subagent.rs
git commit -m "feat: name and track subagent lifecycles"
```

---

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
⠋ Scout · via Pip
  Trace ACP lifecycle                         1m 12s
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
- Produces: user-facing configuration/control documentation and package version `0.1.102`

- [ ] **Step 1: Update user documentation**

Document the exact `[subagent].names` replacement semantics, defaults, empty list, fallback names, reservation-until-close, sibling-only uniqueness, direct-child `subagents({})` listing, observable nested Kit descendants, generic-harness limitation, foreground/background inclusion, row glyphs, footer, `Ctrl+R`, `/agents`, and responsive panel layout.

- [ ] **Step 2: Add documentation assertions where the repository already tests help/config text**

Extend existing command/config documentation tests to assert `/agents`, `Ctrl+R`, and `[subagent].names` examples remain discoverable. Avoid introducing a separate documentation harness.

- [ ] **Step 3: Bump the patch version**

Change the root package version from `0.1.101` to `0.1.102` in `Cargo.toml` and update the root `kit` package entry in `Cargo.lock` through Cargo. Do not change dependency versions.

- [ ] **Step 4: Run formatting and targeted verification**

Run:

```bash
cargo fmt --check
cargo test --bin kit subagent_names -- --nocapture
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
