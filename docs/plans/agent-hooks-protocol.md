# Agent Hooks Protocol integration workflow

## Goal

Integrate the published `agenthooksprotocol` SDK into Kit with broad native lifecycle coverage and modification support. Keep compose as the sole model-exposed tool. Represent the outer compose invocation and its child invocations using AHP event lineage.

Use the SDK's registration, transports, effect reduction, subscription failure policy, content machinery, cancellation, observation delivery, and shutdown. Do not implement competing versions of those facilities. Kit supplies truthful host facts and applies settled effective values to its own execution and presentation paths.

## Integration baseline

External baseline: pinned `agenthooksprotocol =0.4.1`, protocol revision `draft`. Verification of this migration is recorded below; prior SDK verification is not used as evidence for this pin.

Evidence:

- [Published SDK source](https://docs.rs/crate/agenthooksprotocol/0.4.1/source/): `hooks.rs`, `generated.rs`, `runtime.rs`, `client.rs`, `content.rs`, `attachment.rs`, `body.rs`, `hooks_content.rs`, `lineage.rs`, and `adapters/registered.rs`.
- [Pinned protocol schema](https://github.com/agenthooksprotocol/agent-hooks-protocol/tree/05b4750cb2dccf17d75236182d6971f3cbe97e1f).
- Packaged upstream tests: `public_events.rs`, `hooks.rs`, and registered HTTP/process transport tests. These were inspected, not executed.
- Kit's current runtime, observed tool wrappers, provider selection adapter, and compaction implementation.
- AgentKit hook extension: [PR #32](https://github.com/danielkov/agentkit/pull/32), pinned commit `3d75d7179ad6ef4d675d6b9d88cc1864f59be759`. That revision replaces the superseded lifecycle-callback revision `22179a34822660d1bce25fd5b7ec57cf2dd93223` with the reduced interception-hook API: transactional mutation with a canonical rewrite notification, `MutationPoint::TurnStarted`, response disposition, and awaited `NativeDelivery`. Package versions and the transitive set remain unchanged; only the source revision moves.

The SDK has 32 named event boundaries. Tool inputs support explicit event identity, parent event identity, call identity, session, and tool facts. `tool_before_event` exposes the effective event; `tool_before` exposes effective input without the complete event envelope. Registration carries subscription failure policy. Body modifications require authorized content and the SDK canonical content contract; Kit preserves dynamic capability grants without legacy target mappings.

Kit's child dispatcher exposes the owning compose call ID and constructs child requests with empty metadata. Invocation-scoped executor context carries parent event identity without parsing call IDs. The pinned AgentKit extension supplies awaited `LoopMutator` interception hooks at the session-start, model-request, model-response and transcript-mutation boundaries, and awaited read-only `NativeDelivery` of the three facts a mutator cannot carry: streaming progress, the non-editable pre-finish, and the post-commit turn ending. AHP does not need a `LoopObserver` bridge or a copied loop. Transcript rewrites have a dedicated persistence notification. Input admission, session closure, model-failure classification and retry policy are Kit-owned: the loop has no close boundary and reports no model errors, so Kit dispatches inbound prompts after `session.start` through one admission path shared by the runtime and ACP prompt surfaces, wraps its own model adapter to classify only real model failures, and retires any interrupted turn before awaiting `session.end` and SDK shutdown. Because the terminal boundaries are awaited, cooperative cancellation signals the loop's own handle and then awaits the in-flight driver call; only the idle background-task wait, which the loop does not own, is raced against cancellation.

## Integration rules

- Advertise only wired events, modes, and modification targets in the SDK host manifest.
- Use SDK-effective values at the point of consumption, not merely in notifications.
- Use the SDK's settled outcome, diagnostics, and failure policy; do not independently reinterpret backend responses.
- Preserve existing ACP notifications and durable-session behavior while integrating AHP.
- Do not add new tool-call mechanisms or attempt to intercept execution inside external agents.
- Do not treat the integration as a reason to add a new Kit execution-policy subsystem.
- Test Kit's translation and write-back, not the SDK's internal algorithms or transport implementation.

## Implementation and verification status

The native host directly awaits SDK boundaries; it does not use an observer bridge. Implemented boundaries cover session, input, turn, model, tool, compaction, ACP model/config changes, and committed edit notifications. Supported effective values are written back to the actual consumers. Compose child lineage is invocation-scoped and concurrency-safe. See [native AHP configuration](../user/agent-hooks-protocol.md) for exact supported capabilities and limitations.

The SDK 0.4.1 integration uses central dispatch projection and restoration rather
than legacy `content_target` mappings or a native-JSON compatibility workaround.
Native `Item` arrays at `/items` and inbound `/message/messages` are canonical
inline JSON message lists with `id`, `role`, and `parts`. Text is inline plain
text; structured values, tool-call input, and custom actual values are JSON-text
payloads without native wrappers. Media/file parts with actual bytes use immutable
SDK attachments, not a generic prompt attachment. Unresolved native references
and opaque multipart tool results remain immutable, with no implicit fetch.

The read-only `session.end` schema still expects flat parts rather than messages.
Kit flattens that receipt, retains scoped identities, reports missing normalized
message-role facts as a gap, and leaves native transcript values unchanged.

The SDK-effective event restores native values while preserving metadata,
accounting, call identity, and dynamic capability grants. Existing native prompt
and output validation remains authoritative. Projection does not expand supported
capabilities or make read-only boundaries editable. Kit explicitly sets
`max_body_bytes = 4 MiB`, `max_stored_bytes = 64 MiB`, and an attachment-owner
limit of 4,096. Inline text is checked at 4 MiB per native slot before dispatch
and effective writeback; Kit also explicitly sets the SDK backend transport-frame
ceiling to 4 MiB for the complete serialized protocol envelope. These are not
total-RSS bounds. Selected
content may be prepared before settlement denial; denial does not guarantee that
no attachment capture or transport preparation occurred.
The SDK owns transport cancellation, failure policy, effect
reduction, and attachment accounting. Shared reads use the fixed SDK concurrency
support without a Kit serial-read restriction.

The 0.4.0 migration removes the host implementation of the SDK's erased
projection trait and the generic string-based host-input dispatch. Native
projection feeds public domain-named generated inputs; immutable binary sources
enter only schema-owned `HostInput` slots through typed `MessageInput` and
`PartInput` builders. SDK 0.4.1 named boundaries return `EventOutcome<Event>`
and both generic and specialized tool outcomes carry the canonical
`settled_event`. Native restoration consumes that SDK-owned typed event without
re-decoding raw JSON or constructing a replacement SDK outcome. Raw views are
retained intentionally for pointer access and diagnostics, not settlement
normalization. Canonical settled events, raw complete-event inputs, and their
open extras stay lossless. Named input facts expose declared SDK fields
(including `native` and `extensions`), not arbitrary top-level host fields;
opaque nested facts and schema-supported open extras remain intact.
Standalone tool receipts use the public complete-event boundary because `Hooks`
has no named receipt method. The SDK hydrates identity, source, and time before
canonical effective-result decoding. Specialized instructions and summaries use typed
`TextParts`, with no message-list erase/reconstruction roundtrip. Registered-hook
regressions for both specialized edits remain enabled and require successful
settlement; fail-closed delivery is not weakened.

### Verification of SDK 0.4.1 typed settlement

Checks used `CARGO_INCREMENTAL=0`, `MISE_OFFLINE=true`, the existing toolchain
data directory, isolated HOME/XDG configuration, and no inherited live
subagent-tree slot directory. Logs are under `/tmp/kit-ahp-sdk041`.

- `mise run test -- --lib ahp::`: 58 passed, including specialized inline text,
  native writeback, opaque extensions/open complete-event extras, caller
  input/result/provenance contracts, decode failure evidence, and typed accepted
  response envelopes.
- `mise run test -- --lib compaction`: 33 passed.
- `mise run test -- --lib tools::mcp::`: 100 passed.
- `mise run test -- --lib`: 1,784 passed, seven ignored, zero failures and no
  filtered tests.
- `mise run test -- --test ahp_herdr --no-run`: compiled the opt-in integration.
- `mise run lint`: formatting and strict all-target Clippy passed with all
  features and with no default features.

The final live HerdR smoke passed with Kit using SDK 0.4.1 and the existing
installed SDK 0.4.0 bridge: `python3 scripts/ahp-herdr-smoke.py --bridge
/Users/danielkov/.agents/hooks/herdr-ahp`. Isolated session
`kit-ahp-e2e-1033d3fd62` reported idle → working → idle, and stop/delete cleanup
both succeeded. Evidence: `/var/folders/t9/w5z6dqrs68nfb_spxn62h32w0000gn/T/kit-ahp-herdr-live-dfkx7y10/evidence.json`.
No external bridge repository or installed version was changed.

### Previous SDK verification (0.4.0)

Checks against SDK 0.4.0 used `CARGO_INCREMENTAL=0`, `MISE_OFFLINE=true`, the
existing toolchain data directory, isolated HOME/XDG configuration, and no
inherited live subagent-tree slot directory:

- `mise run lint`: format checks and strict all-target Clippy passed with all
  features and with no default features.
- Focused AHP tests: 53 passed, including both retained specialized
  instructions/summary regressions.
- Focused compaction tests: 33 passed. Focused MCP tests: 100 passed.
- Full isolated library suite: 1,779 passed, seven ignored, zero failures and zero
  filtered tests. No specialized regression was excluded.
- `mise run test -- --test ahp_herdr --no-run`: the opt-in live integration compiled.

The final isolated live HerdR smoke also passed with a temporary bridge built
against SDK 0.4.0: initial idle, turn working, and final idle were observed;
stop/delete cleanup succeeded. This does not claim that every integration suite
passed.

The previous full integration run encountered an unrelated gateway fixture that
reuses `discover-mcp` as the tool-call identity across prompts; native transcript
validation rejects that duplicate. This migration does not weaken call identity
validation or change that fixture. The live bridge smoke is separate from
library/lint verification. No real bridge repository dependency pins were
changed.

Known scope limits remain unchanged: immutable binary originals are not
hook-editable bytes; unsupported native model/config modification targets remain
observation/denial only. Fresh subprocesses discover global/project hook files
rather than inheriting programmatic overrides. Aborted-driver cleanup remains
best-effort. The integration does not support all SDK event boundaries.

## Workflow

### 1. Produce an event-to-boundary map

For every named SDK event, record the actual Kit operation, timing, facts available, supported modes, modification target, and destination for effective values.

Start with these existing seams:

| Event group | Kit boundary to inspect and wire |
| --- | --- |
| Session start/end | Native session creation, resume, and teardown |
| User messages and turns | Input acceptance, turn submission, final response settlement and delivery |
| Model request/response/error | Model session and turn adapter boundaries |
| Model switch | Provider selection replacement |
| Tool before/after | Outer compose execution and registered child invocation |
| Tool batch/progress | Actual compose dispatch/completion and progress information |
| Context compaction | Manual and automatic compaction preparation and installation |
| Config/workspace/task/file changes | Actual committed changes owned by Kit |
| Attention/elicitation/permission | Corresponding real interaction, if one exists |
| Hook failure | SDK diagnostics and the contract's failure-event behavior |

Map both persistent and ephemeral native runs and subagent/fork session construction. Distinguish a durable session from a single prompt run; do not emit session end after every turn. Distinguish intermediate model responses from final user-visible response delivery.

Do not synthesize permission, attention, elicitation, task, or change events merely to complete the catalogue. Broad coverage means wiring real operations, not pretending Kit has every optional workflow.

Exit: a complete inventory with concrete write-back points. Any missing pre-consumption seam must have a minimal adapter or upstream interface proposal before its event is advertised.

### 2. Add the SDK and ownership/configuration plumbing

- Pin the published SDK version and enable its `reqwest` and `tokio-process` features for built-in HTTP and stdio registration.
- Apply Kit's secure Rust dependency-change workflow, including transitive dependency and duplicate-version review.
- Discover canonical SDK registrations in `~/.agents/hooks.json` and `<root>/.agents/hooks.json`; merge hooks by ID with whole-definition project-local precedence.
- Derive host event grants from implemented Kit support and construct `Hooks` before native execution begins.
- Keep the disabled path unchanged; no hook registration means no transport work.
- Define ownership so every admitted native session has access to its hook service and every owned service reaches SDK shutdown before runtime teardown. Do not shut down a shared service when one session ends.
- Use the SDK's credential facilities; add a Kit credential-provider bridge only if the supported registration requires it.

Exit: registrations initialize through real SDK APIs; both built-in transports are available; ownership and shutdown are wired. No Kit transport factory is required.

### 3. Wire compose and children with lineage and modification

- Emit and await the outer compose `tool.before` before its effective program/input is consumed.
- Retain the outer before-event ID, using `tool_before_event` or an explicit supplied ID.
- Carry that event ID through child dispatch. Use the dispatcher's explicit parent-call association rather than parsing generated call-ID strings.
- Set each child's `parentEventId` to the owning compose before-event ID. Keep call identity distinct from event identity and preserve the source namespace.
- Await child `tool.before`, then execute its effective decoded input.
- Bind child completion to its matching before event and outer completion to the outer invocation. Apply output modifications before results enter Runlet or the model-facing compose result.
- Cover both invocation APIs, dynamically registered tools, background compose work, and cancellation without duplicate boundaries.
- Keep per-invocation lineage attached to the invocation lifetime; concurrent compose runs must not share mutable current-parent state.
- Keep content on the invocation lifetime: canonical inline message parts for editable values and immutable SDK attachments for actual media/file bytes. Restore SDK-effective values before native consumption. Never raise a configured budget or suppress a capacity error to make room.

First implementation spike: prove how the parent event context reaches a child wrapper. Existing child metadata does not carry it. Prefer an existing executor/context seam; if none supports reliable propagation across owned/background work, make a narrowly scoped AgentKit interface change. Do not infer that task-local context automatically survives spawned work.

Exit: one real compose program produces outer and child events with correct lineage, modified child arguments reach execution, and modified results reach the caller.

### 4. Wire session, turn, message, and model lifecycle

- Install awaited boundaries at native session and turn entry/exit, inbound message acceptance, and outbound delivery.
- Wrap the actual model request/response consumption seams; do not use post-hoc observer notifications for interception.
- Apply effective requests and body-backed modifications before the provider or downstream transcript consumes them.
- Project canonical inline messages through central SDK dispatch and restore effective native values. Preserve semantic structure, dynamic capability grants, metadata, accounting, and call identity without legacy `content_target` mappings.
- Wire model switching around actual adapter replacement and errors at the boundary that owns them.
- Use awaited `LoopMutator` interception and read-only `NativeDelivery` for progress and terminal facts. Do not infer logical turns from provider requests or only from public `LoopStep::Finished` results. Preserve ordered delivery and explicit teardown ownership; no synchronous observer bridge or copied loop is needed.

Exit: hook-modified prompt/request values reach the provider; modified responses reach their intended downstream consumer; native entry points do not silently omit hooks.

### 5. Wire compaction and remaining real lifecycle events

- Use the SDK's compaction contract around both manual and automatic compaction.
- Translate candidate context/summary content and install the settled effective result at the actual transcript replacement boundary.
- Wire real configuration, workspace, task, file, progress, and interaction events identified in step 1.
- Advertise each event and effect target when its full mapping is implemented, including write-back where applicable.

Exit: broad catalogue coverage is documented against actual Kit operations, with no advertised modification that is ignored.

### 6. Validate the Kit integration and document usage

Focused integration checks through real Kit APIs and a small external hook fixture:

- Unconfigured Kit retains existing behavior.
- A child-input modification changes what the actual tool receives.
- Output and prompt/response modifications reach the correct downstream consumer.
- Concurrent children and overlapping/background compose runs retain the correct parent identity.
- Session/turn pairs and compaction boundaries correspond to actual operations.
- SDK-settled non-executable outcomes are not accidentally executed by Kit.
- Native shutdown calls SDK cleanup through the established ownership path.

Do not duplicate SDK tests for effect validation, JSON merging, HTTP framing, subprocess management, or fail-open/fail-closed reduction. Transport smoke checks only establish that Kit's configuration connects to the SDK correctly.

Run the smallest relevant checks through `mise`, followed by applicable lint/build checks. Document registration examples for both SDK transports, available events and modifications, lineage, credential references, and the pinned draft protocol version.

## Delivery sequence

1. Boundary map and parent-context spike.
2. Dependency/configuration/ownership foundation plus compose and child interception/modification.
3. Session, turn, message, and model lifecycle with effective-value write-back.
4. Compaction and remaining real lifecycle coverage; focused integration checks and user documentation.

Keep upstream interface changes separate if the spike proves they are needed. Do not build speculative abstractions for gaps that have not been demonstrated.

## Completion criteria

- Compose remains the only model-exposed tool.
- Outer compose and all invoked children have truthful, concurrency-safe AHP lineage.
- Advertised modification targets alter the values Kit actually consumes.
- Registered transports and failure semantics are provided by the SDK without competing Kit machinery.
- Supported lifecycle events are wired across native execution paths and accurately advertised.
- Existing ACP projection and durable-session behavior remain intact.
- Validation concerns Kit's integration, not independent reimplementation testing of the third-party SDK.
