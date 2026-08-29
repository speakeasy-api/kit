# Named Subagent Activity Roster Design

## Summary

Kit will give parent-owned subagents stable, friendly display names and show every observable foreground, background, and nested descendant in a dedicated always-expanded tree TUI Agents panel. Kit randomly selects each new handle's one-word display name from a curated built-in catalog of exactly 350 whimsical names; callers and configuration do not supply names. The existing immutable `s-…` ID remains authoritative.

The feature is backed by each Kit process's in-process subagent registry and recursively forwarded private runtime events. It does not claim that ACP provides portable parent/child session enumeration.

## Goals

- Make concurrent subagents easy to recognize in a dedicated always-expanded tree TUI panel.
- Include foreground and background work independently of the currently focused tool call.
- Aggregate observable nested descendants and show their immediate parent identity.
- Keep one display identity across repeated prompts to the same reusable handle.
- Generate short, whimsical one-word names without configuration or model input.
- Keep random generation bounded and retain a deterministic emergency fallback.
- Expose useful direct-child lifecycle metadata through `subagents({})`.
- Preserve compatibility with existing configuration and handles.

## Non-goals

- Defining a new portable ACP child-session protocol.
- Enumerating subagents owned by unrelated top-level Kit sessions or process trees.
- Discovering private internal agents created by generic ACP harnesses that do not emit Kit runtime events.
- Restoring live child processes after Kit restarts.
- Replacing immutable subagent IDs with display names in tool inputs.
- Enforcing display-name uniqueness across separate descendant branches.
- Changing the existing runtime graph's contents or `Ctrl+G` behavior.

## Background and constraints

ACP v2 can list persisted sessions and report foreground session state, but its session records do not define parent IDs, subagent identity, tasks, or child lifecycle. Kit's generic child harness also currently uses ACP v1. ACP session enumeration is therefore not a sufficient roster source.

Kit already owns authoritative parent-scoped state in `src/tools/subagent.rs::Subagents.sessions`. The TUI already receives private structured events from `src/events.rs::RuntimeEvent` and applies them in `src/tui/app.rs`. The feature will extend these existing paths.

## Name assignment and ownership

Kit does not expose name configuration and does not ask a model or other AI to choose names. Every new `subagent` and `fork` independently selects a name at random from a compile-time built-in catalog of exactly 350 reviewed, whimsical codenames spanning cosmic, botanical, food, music, craft, weather, and playful themes. Every catalog entry is an ASCII alphabetic word of 1–16 characters and is unique case-insensitively.

Names compare case-insensitively among one parent's direct live children. A collision uses the lowest available suffix (`Name 2`, `Name 3`, and so on), shortening the base if needed to keep the label within 32 Unicode scalar values. If bounded generation cannot produce an available valid candidate, Kit uses the lowest available `Agent N` label.

The reservation is created atomically with the starting registry entry. It remains held while the handle is starting, working, idle, or reusable after a non-terminal failure. Failed initial creation, explicit close, and terminal retirement remove the registry entry and release the name. Separate parent branches may reuse visible labels because immutable `s-…` IDs remain authoritative. `prompt` preserves the source handle's name; `fork` creates a distinct identity and allocates a fresh random name.

## Lifecycle model

The live registry lifecycle is:

```text
starting -> working -> idle -> working -> idle
    |          |        |
    +----------+--------+-> removed
```

- `starting`: the handle and name are reserved while its child session is being created.
- `working`: the child is processing the current generation.
- `idle`: the generation finished and the handle can be prompted, forked, or closed.
- `removed`: explicit close, failed initial creation, or terminal retirement makes the handle unusable and releases its name. `removed` is an event tombstone, not a live listing status.

A successful generation transitions `working -> idle` with a successful outcome. A reusable generation failure also transitions `working -> idle`, records a failed outcome timestamp, and remains counted as idle. The TUI renders that idle row with a red `✗` for four seconds, then with the ordinary dim `○`.

A terminal generation failure retires the handle, releases its name and capacity, and emits a failed `removed` tombstone. The TUI excludes it from live footer counts, retains a red `✗` tombstone for four seconds, then deletes the row. Explicit close emits a non-failed `removed` tombstone and deletes the row immediately. Failed initial creation releases its reservation and emits a failed `removed` tombstone under the same four-second display rule.

Lifecycle changes occur under the same synchronization that protects the corresponding subagent state so listing and emitted events cannot report a transition that did not take effect.

## Data model

The private subagent state gains:

- assigned display name;
- explicit live lifecycle status;
- bounded current-task summary;
- current generation;
- current generation outcome; and
- Unix-millisecond timestamps needed for elapsed-time and failure-grace display.

The registry map remains keyed by immutable subagent ID. Name availability is derived or maintained under the registry's existing synchronization so concurrent starts cannot claim the same name. Assignment and insertion must be atomic from the perspective of other starts and listings.

Task summaries are deterministic, bounded, single-line renderings of the operation prompt. Kit replaces each run of Unicode whitespace, including newlines, with one ASCII space and trims the result. Length is counted in Unicode scalar values (Rust `char` values). If the normalized summary exceeds 96 scalar values, Kit keeps the first 95 and appends `…`, producing a maximum of 96. An empty normalized prompt is displayed as `Untitled task`. The full prompt is not copied into roster events. Existing tool input and transcript behavior remains unchanged.

## Tool API

### `subagent`

The existing input remains unchanged for naming: callers provide the task prompt and any supported harness/model/output-schema selection. There is no name field. Kit allocates the display name internally before child startup.

### `fork`

The existing fork input remains unchanged for naming and has no name field. The source keeps its name; the fork receives a newly allocated random name.

### `prompt`

`prompt` continues to accept the prior handle plus the next prompt and preserves the authoritative registry name. A caller-supplied informational `name` in a serialized handle never changes identity or naming.

### Returned handles

Newly returned `SubagentValue` objects include `name`:

```json
{
  "id": "s-…",
  "name": "Scout",
  "output": "…",
  "generation": 1,
  "updates": { "items": [], "truncated": false }
}
```

Continuation tools continue to resolve the registry entry by `id`. They accept older serialized handles that omit `name`; any supplied name is informational and cannot rename or redirect a handle. New outputs always include the registry's authoritative name.

Because handles can appear in persisted transcripts, the deserializer and JSON Schema must keep `name` optional on input for backward compatibility even while current outputs populate it.

### `subagents` listing

`subagents({})` returns parent-owned live identities with roster metadata:

```json
[
  {
    "id": "s-…",
    "name": "Scout",
    "status": "working",
    "generation": 2,
    "task": "Trace the graph event flow"
  },
  {
    "id": "s-…",
    "name": "Pip",
    "status": "idle",
    "generation": 1,
    "task": "Inspect ACP session capabilities"
  }
]
```

Listings are deterministic, ordered by creation time and then ID as a tie-breaker. Closed or retired handles are absent. The richer listing replaces the current untagged starting-versus-ready representation; callers should use `status` to interpret lifecycle. The exact output schema documents all statuses.

## Runtime events

The private `RuntimeEvent` protocol gains a structured subagent transition event with these conceptual fields:

```rust
SubagentStateChanged {
    id: String,
    name: String,
    status: SubagentStatus,
    outcome: Option<GenerationOutcome>,
    generation: u64,
    task: String,
    parent_id: Option<String>,
    parent_name: Option<String>,
    harness: String,
    model: Option<String>,
    created_at_unix_ms: u64,
    generation_started_at_unix_ms: u64,
    generation_finished_at_unix_ms: Option<u64>,
}
```

`SubagentStatus` contains `starting`, `working`, `idle`, and the non-live `removed` tombstone. `GenerationOutcome` is `success` or `failed`. All serialized times are Unix epoch milliseconds so they remain meaningful across process boundaries. For a live duration, the TUI subtracts `generation_started_at_unix_ms` from its current Unix-millisecond clock and clamps negative results to zero. For a finished generation it subtracts the start from `generation_finished_at_unix_ms`.

Direct children of the top-level runtime omit `parent_id` and `parent_name`. When Kit launches a child Kit runtime, it passes that child handle's immutable ID and display name as internal parent context. Lifecycle events emitted by that runtime for its own direct children include the immediate parent context. Existing child stderr event forwarding is extended to forward subagent transition events unchanged, so ancestry remains intact across multiple levels.

The top-level TUI deduplicates recursively forwarded events by immutable agent ID. IDs are globally unique enough for the existing process/session model; display names are not used as keys. Generic ACP harnesses that do not run Kit or emit Kit runtime events cannot expose their private internal agents.

A second tombstone event, `SubagentDescendantsRemoved { ancestor_id }`, handles recursive cleanup. The ACP child wrapper emits it when a child Kit process or its runtime-event forwarding channel exits, whether orderly or unexpectedly. On receipt, each parent forwards it unchanged. The top-level TUI removes every strict descendant whose recorded parent chain contains `ancestor_id`, but does not remove the ancestor row itself. The ancestor remains governed by its own direct registry transition: reusable failure returns it to idle, while close or terminal retirement emits `removed`. Cleanup is idempotent by immutable ID.

Events are emitted after successful state transitions. They are emitted whether the initiating compose/tool call remains foregrounded or has been backgrounded. Event delivery is observational: serialization, transport, parsing, forwarding, or rendering failure must not change tool behavior. Existing event-marker framing and opt-in `KIT_RUNTIME_EVENTS` transport are retained.

The ACP-facing transcript is not extended with a pretend portable child-session type. Tool inputs and outputs continue to flow through ACP normally; lifecycle detail remains on Kit's private runtime channel.

## TUI Agents panel

The roster is a dedicated always-expanded tree panel, independent from the existing graph.

### Visibility and layout

- `Ctrl+R` toggles the Agents panel.
- `/agents` performs the same toggle and makes the feature discoverable through local command help.
- `Ctrl+G` and the graph remain unchanged.
- The transcript, graph, and Agents panel retain independent visibility and scroll state.
- Graph and Agents each use the existing 46-column side-panel width.
- With exactly one side panel visible, widths at or above the existing 108-column breakpoint render transcript then side panel horizontally. Below 108 columns, they render top-to-bottom as transcript then side panel using the existing 55/45 percent split.
- With both side panels visible, widths at or above 154 columns render three full-height columns in this order: transcript, graph, Agents. The two side panels are 46 columns each and the transcript receives the remainder.
- With both side panels visible below 154 columns, the body renders vertically in this order: transcript, graph, Agents, using a 40/30/30 percent height split. The outer TUI already guarantees the body area; each panel preserves its border and handles zero-width or zero-height inner content with the existing saturating layout conventions.

The Agents panel is global to the current top-level process tree. Its contents do not depend on the focused transcript block, foreground tool call, or graph selection. Mouse-wheel scrolling over the panel changes only its own offset. Active rows sort before idle rows; starting sorts before working, and each status group uses creation time then immutable ID for deterministic ordering.

### Rows

Each agent occupies two terminal lines:

```text
○ Pip
│  └─ ⠋ Scout
│     Trace ACP lifecycle                     1m 12s
```

The first line contains the state indicator and display name. Direct agents are roots and descendants render immediately beneath their parent at arbitrary depth. Tree connectors and indentation continue across both lines. A row whose parent has not arrived temporarily renders as a root with ` · via Parent`, then automatically reparents when the parent event arrives. The second line contains the bounded task summary and the current generation's elapsed time. Elapsed time is right-justified against the panel's inner edge. Task text truncates before the reserved duration column and never shifts the duration.

Rows do not print textual status labels. State is communicated by the selected Kit-native indicator palette:

- `starting`: yellow `Pulse::Child`, the slow orbiting child pulse;
- `working`: cyan `Pulse::Tool`, the familiar tool spinner;
- successful or ordinary `idle`: dim static `○`;
- failed reusable `idle`: red static `✗` for four seconds after its failure timestamp, then dim `○`;
- failed `removed` tombstone: red static `✗` for four seconds after its failure timestamp, then row deletion.

Starting and working durations update on animation ticks. Elapsed time measures the current generation from its initial `starting` transition and freezes when the generation becomes idle or reaches terminal failure. A later prompt starts a new duration for the new generation. The four-second failure grace affects only glyph rendering and tombstone retention, not lifecycle status or footer counts.

### Aggregate footer

A fixed footer remains visible while rows scroll and shows the total plus nonzero lifecycle buckets:

```text
3 agents · 2 working · 1 idle
```

or:

```text
5 agents · 1 starting · 3 working · 1 idle
```

Foreground and background are deliberately not separate buckets. A failed-but-reusable handle counts as idle immediately after its transition back to idle, including during its red-glyph grace period. Explicitly closed and terminally retired handles are excluded from the live total immediately, even if a failed tombstone remains visible for four seconds.

### Closing and history

Idle handles remain visible until explicitly closed. Closing removes the live row and releases its locally reserved name. The Agents panel is live process state, not a historical operation log; completed transcript and tool output history remains unchanged.

## Persistence and restart behavior

Live roster state is process-local and is not restored after a restart. Kit does not resurrect child ACP processes. Historical tool outputs may retain the assigned name through `SubagentValue`, but reconstructing full runtime child timing and lifecycle history remains outside this feature.

The optional serialized handle field is a backward-compatible schema addition. Implementation must follow Kit's persistent-artifact schema workflow and explicitly test old handles without `name`. The unreleased `names` configuration experiment is removed and explicitly rejected so stale development configuration does not appear to work.

## Error handling

- Random catalog selection is retried within a fixed bound if entropy is unavailable or a selected base cannot be used.
- Exhausted random retries fall through to the lowest available `Agent N`; naming never blocks useful work.
- A failed initial child creation removes its registry entry and releases its name and capacity.
- A failed reusable generation records the failed operation and returns the identity to idle.
- A terminally retired child is omitted from listings and releases its name.
- Child-process or forwarding-channel exit emits descendant cleanup so stale nested rows do not survive their owning subtree.
- Unknown or stale IDs retain existing tool errors.
- Runtime-event failures do not fail subagent operations.

## Testing strategy

Focused tests will cover:

1. Exactly 350 ASCII alphabetic catalog entries, 1–16 characters long and case-insensitively unique.
2. Random catalog selection, collision suffixing, bounded retries, and emergency `Agent N` fallback.
3. Atomic case-insensitive allocation and collision suffixes under concurrent sibling starts.
4. Release after failed creation, explicit close, and terminal retirement.
5. Name preservation across `prompt` and fresh random assignment across `fork`.
7. Starting, working, idle, failed-reusable, retired, and closed transitions.
8. Current handles with names and compatibility with old handles lacking names.
9. Deterministic enriched direct-child `subagents({})` listings.
10. Runtime-event serialization, parsing, background delivery, and transition ordering.
11. Parent-context propagation and recursive forwarding across nested Kit runtimes.
12. Top-level deduplication by immutable ID and strict-descendant cleanup after child or forwarding exit.
13. Duplicate visible labels across branches remain separate internal rows.
14. Reusable and terminal failure glyph grace, row retention, and footer accounting.
15. `Ctrl+R` and `/agents` toggling without changing `Ctrl+G`.
16. Two-column, three-column, and exact 40/30/30 narrow vertical layouts.
17. Two-line rows, palette A indicators, right-justified duration, truncation, sorting, scrolling, and aggregate footer counts.
18. Config documentation examples and relevant schema/snapshot expectations.

The smallest relevant unit and TUI tests should run during development, followed by the repository's standard targeted verification for the touched crates.

## Documentation and release

Update the user documentation for subagents and the TUI to explain random names, emergency fallback behavior, `Ctrl+R` and `/agents`, lifecycle indicators, foreground/background inclusion, observable nested descendants, ancestry-scoped uniqueness, and process-tree scope.

This feature meaningfully changes Kit behavior and tool schemas. Under repository policy it requires a patch version bump before completion.

## Future extensions

Possible follow-up work includes a machine-wide daemon-backed roster, persisted runtime history, strict tree-wide name coordination, keyboard-focused roster navigation, filtering or sorting controls, and a standardized ACP proposal for child-session relationships. None are required for this design.
