# Named Subagent Activity Roster Design

## Summary

Kit will give parent-owned subagents stable, friendly display names and render their lifecycle directly in the existing TUI graph. Names come from a configurable pool first, then from an optional fallback suggested by the parent model without an extra model call. The existing immutable `s-…` ID remains authoritative.

The feature is backed by Kit's in-process subagent registry and private runtime-event channel. It does not claim that ACP provides portable parent/child session enumeration.

## Goals

- Make concurrent subagents easy to recognize in the TUI.
- Keep one display identity across repeated prompts to the same reusable handle.
- Let users replace Kit's built-in whimsical name pool.
- Allow the parent model to suggest additional names after the pool is exhausted without a dedicated naming request.
- Expose useful parent-scoped lifecycle metadata through `subagents({})`.
- Associate an agent with the exact `subagent`, `prompt`, or `fork` runtime operation that initiated its current generation.
- Preserve compatibility with existing configuration and handles.

## Non-goals

- Defining a new portable ACP child-session protocol.
- Enumerating subagents owned by unrelated Kit processes or top-level sessions.
- Restoring live child processes after Kit restarts.
- Replacing immutable subagent IDs with display names in tool inputs.
- Adding a separate roster panel, mode, or keyboard shortcut in the first version.
- Reworking identity and attribution for every non-subagent graph node.

## Background and constraints

ACP v2 can list persisted sessions and report foreground session state, but its session records do not define parent IDs, subagent identity, tasks, or child lifecycle. Kit's generic child harness also currently uses ACP v1. ACP session enumeration is therefore not a sufficient roster source.

Kit already owns authoritative parent-scoped state in `src/tools/subagent.rs::Subagents.sessions`. The TUI already receives private structured events from `src/events.rs::RuntimeEvent` and applies them in `src/tui/app.rs`. The feature will extend these existing paths.

## Naming configuration

The existing singular `[subagent]` configuration table gains an optional `names` list:

```toml
[subagent]
harness = "acp.kit"
names = [
  "Scout",
  "Pip",
  "Juniper",
  "Miso",
  "Clover",
  "Pixel",
  "Pebble",
  "Nova",
]
```

When `names` is absent, Kit uses the compiled list shown above. When it is present, the configured list replaces the compiled defaults rather than extending them. An explicitly empty list is valid and proceeds directly to fallback assignment.

Configured names are normalized by trimming surrounding whitespace, then validated. A configured name must:

- be non-empty;
- be a single line and contain no control characters;
- be at most 32 Unicode scalar values (Rust `char` values); and
- be unique case-insensitively after normalization.

Invalid configuration fails during config loading with a message identifying the offending value or duplicate.

## Name assignment and ownership

A newly created identity is assigned a name in this order:

1. The first configured or built-in name not reserved by another live handle.
2. The operation's optional model-suggested `fallback_name`.
3. A generated `Agent N` name.

Pool order is deterministic. A name is reserved as soon as creation begins. If child creation fails, Kit releases the reservation. The reservation otherwise lasts through starting, working, idle, and failed-but-reusable generations and is released only when the handle is closed or terminally retired.

Names compare case-insensitively for collision purposes. If a valid fallback name is occupied, Kit appends ` 2`, ` 3`, and so on, choosing the lowest available positive suffix. `Agent N` likewise uses the lowest positive integer that produces an available name, so released numbers may be reused. The base is safely shortened when necessary so the final display name remains within 32 Unicode scalar values.

Model-provided fallback values are best-effort metadata and must not prevent subagent work. Kit trims surrounding whitespace, collapses repeated internal whitespace, and discards empty, multiline, control-character-containing, or otherwise invalid suggestions. A discarded or absent suggestion falls through to `Agent N`. Unicode names are supported.

`prompt` reuses the handle's existing name. `fork` creates a distinct handle and receives a new name through the normal assignment order. Names are never accepted as identity selectors; `id` remains authoritative.

## Lifecycle model

The displayed lifecycle is:

```text
starting -> working -> idle -> working -> idle -> closed
```

- `starting`: the handle and name are reserved while its child session is being created.
- `working`: the child is processing the current generation.
- `idle`: the generation finished and the handle can be prompted, forked, or closed.
- `closed`: the handle is unusable and its name is released. Closed identities are removed from live annotations.

A generation failure returns the handle to `idle` when the underlying child remains reusable. A terminal child failure retires the handle and releases its capacity and name. Lifecycle changes occur under the same synchronization that protects the corresponding subagent state so listing and emitted events cannot report a transition that did not take effect.

## Data model

The private subagent state gains:

- assigned display name;
- explicit lifecycle status;
- bounded current-task summary;
- current generation; and
- timestamps needed for elapsed-time display.

The registry map remains keyed by immutable subagent ID. Name availability is derived or maintained under the registry's existing synchronization so concurrent starts cannot claim the same name. Assignment and insertion must be atomic from the perspective of other starts and listings.

Task summaries are deterministic, bounded, single-line renderings of the operation prompt. Kit replaces each run of Unicode whitespace, including newlines, with one ASCII space and trims the result. Length is counted in Unicode scalar values (Rust `char` values). If the normalized summary exceeds 96 scalar values, Kit keeps the first 95 and appends `…`, producing a maximum of 96. An empty normalized prompt is displayed as `Untitled task`. The full prompt is not copied into roster events. Existing tool input and transcript behavior remains unchanged.

## Tool API

### `subagent`

The input schema gains an optional `fallback_name` string:

```json
{
  "prompt": "Trace the graph event flow",
  "fallback_name": "Waffles"
}
```

The tool description asks the parent model to provide a short whimsical fallback. Kit uses it only after the configured/default pool is exhausted. No extra model request is made.

### `fork`

Because `fork` creates a distinct identity, its input schema gains the same optional `fallback_name` field. The name of the source handle is preserved only on the source; the fork receives a newly allocated name.

### `prompt`

`prompt` does not accept a naming field. It retains the target handle's assigned name.

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
    generation: u64,
    task: String,
    operation_call: String,
    harness: String,
    model: Option<String>,
    at: u64,
}
```

`operation_call` is the unique runtime child-call ID for the `subagent`, `prompt`, or `fork` operation that initiated the generation. This lets the TUI decorate the exact child record rather than matching only by tool name.

Events are emitted after successful state transitions. Event delivery is observational: serialization, transport, parsing, or rendering failure must not change tool behavior. Existing event-marker framing and opt-in `KIT_RUNTIME_EVENTS` transport are retained.

The ACP-facing transcript is not extended with a pretend portable child-session type. Tool inputs and outputs continue to flow through ACP normally; lifecycle detail remains on Kit's private runtime channel.

## TUI activity graph

The existing graph becomes an activity graph without adding a separate mode. `Ctrl+G` retains its current behavior.

Subagent operations render inline:

```text
◆ ACTIVITY
┌ compose                                      ● running
├─ subagent -> Scout                           ● working
│  └ Trace ACP lifecycle · gen 1 · 1m 12s
├─ subagent -> Pip                             ● working
│  └ Inspect graph event flow · gen 1 · 48s
├─ shell                                       ✓ 1.2s
└─ fork Scout -> Miso                          ◐ starting
   └ Try the alternate approach · gen 1
```

A re-prompt shows the retained identity and new generation:

```text
├─ prompt -> Scout                             ● working
│  └ Verify the proposed event model · gen 2
```

Status presentation is:

- `starting`: yellow half-filled indicator;
- `working`: active green/accent indicator;
- `idle`: dimmed hollow indicator;
- failed generation: red outcome on that operation, followed by idle when reusable.

The TUI maintains live agent state keyed by immutable ID. When an agent becomes idle, its most recent operation remains visible with a dimmed live annotation. A new prompt moves the live annotation to the new operation; older operations remain ordinary history. Closing removes the live annotation and releases the name, while completed graph history retains the name and outcome recorded for that operation.

Collapsed rows show name, bounded task, state, generation, and elapsed time. Existing expansion behavior exposes immutable ID, harness/model when relevant, and existing result or error detail.

Elapsed time measures the current generation's operation from its initial `starting` transition. It updates while the operation is starting or working and freezes when that generation reaches idle or a terminal failure. A later prompt starts a new elapsed duration for the new generation.

At narrow widths, existing graph layout and truncation rules apply. Names and task summaries truncate before status indicators so lifecycle remains readable.

## Persistence and restart behavior

Live roster state is process-local and is not restored after a restart. Kit does not resurrect child ACP processes. Historical tool outputs may retain the assigned name through `SubagentValue`, but reconstructing full runtime child timing and lifecycle history remains outside this feature.

The optional config field and optional serialized handle field are backward-compatible schema additions. Implementation must follow Kit's persistent-artifact schema workflow and explicitly test old handles without `name`. No config migration is necessary.

## Error handling

- Invalid or duplicate configured names fail config loading with precise errors.
- Invalid model fallbacks silently fall through to `Agent N`; naming metadata never blocks useful work.
- A failed initial child creation removes its registry entry and releases its name and capacity.
- A failed reusable generation records the failed operation and returns the identity to idle.
- A terminally retired child is omitted from listings and releases its name.
- Unknown or stale IDs retain existing tool errors.
- Runtime-event failures do not fail subagent operations.

## Testing strategy

Focused tests will cover:

1. Built-in defaults, custom replacement, and an empty pool.
2. Whitespace normalization, invalid configured names, and case-insensitive duplicates.
3. Atomic deterministic allocation under concurrent starts.
4. Release after failed creation, explicit close, and terminal retirement.
5. Pool exhaustion, valid and invalid fallback names, collision suffixes, and length limits.
6. Name preservation across `prompt` and fresh assignment across `fork`.
7. Starting, working, idle, failed-reusable, retired, and closed transitions.
8. Current handles with names and compatibility with old handles lacking names.
9. Deterministic enriched `subagents({})` listings.
10. Runtime-event serialization, parsing, and transition ordering.
11. Exact association between operation-call IDs and graph child rows.
12. Activity rendering, status styling, expansion details, and narrow-width truncation.
13. Config documentation examples and relevant schema/snapshot expectations.

The smallest relevant unit and TUI tests should run during development, followed by the repository's standard targeted verification for the touched crates.

## Documentation and release

Update the user documentation for subagents/configuration and the TUI graph to explain names, fallback behavior, lifecycle states, and process-local scope.

This feature meaningfully changes Kit behavior and tool schemas. Under repository policy it requires a patch version bump before completion.

## Future extensions

Possible follow-up work includes a machine-wide daemon-backed roster, persisted runtime history, filtering or sorting controls, and a standardized ACP proposal for child-session relationships. None are required for this design.
