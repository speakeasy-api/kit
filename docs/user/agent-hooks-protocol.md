# Native Agent Hooks Protocol

Kit automatically reads canonical AHP registrations from `~/.agents/hooks.json`
(global) and `<root>/.agents/hooks.json` (project-local). No `[ahp]` TOML table is
required. If neither file exists, hooks are disabled. Registration validation,
effect reduction, and registered transports remain owned by the AHP SDK.

## Configure hooks

Put a canonical registration directly in either `hooks.json` file:

```json
{
  "protocolVersion": "draft",
  "hooks": [
    {
      "id": "org.example.local-policy",
      "transport": {
        "type": "stdio",
        "command": "python3",
        "args": ["/absolute/path/to/policy.py"],
        "lifecycle": "persistent"
      },
      "subscriptions": [
        {
          "events": ["tool.before"],
          "mode": "intercept",
          "timeoutMs": 3000,
          "failurePolicy": "fail-closed",
          "content": {"default": "metadata"}
        }
      ]
    }
  ]
}
```

### Global and project merge

Kit loads the global file first, then the project-local file. Hooks merge by
`id`: a project-local hook replaces the entire global definition with the same
ID. Other global hooks remain active, and new project hooks are appended in
file order. Duplicate IDs within one file are invalid. If both paths identify
the same file, it is loaded only once.

Missing files are allowed. Unreadable files, malformed JSON, and invalid SDK
registrations produce a warning identifying the file; Kit skips that file and
continues with any valid configuration from the other scope. An incompatible
local protocol version leaves the valid global registration in effect. If no
valid file remains, hooks are disabled rather than preventing Kit startup.
Neither file is rewritten. If `HOME` is unset or empty, only the local
file is considered. The local path is relative to Kit's selected project root
(including `--root`), not necessarily the shell's working directory.

Kit supplies its implemented event modes and capabilities to the SDK; no
separate `grants` object is required in the file. Subscriptions and SDK content
selection still determine participation and available modification targets.
Unsupported host capabilities are not advertised. Use the SDK's
environment-backed credentials where supported rather than inline bearer secrets.
Use absolute executable/script paths for predictable behavior across sessions.

Hook configuration is executable policy: a stdio backend runs a local process,
and an HTTP backend can receive subscribed event content. Review both global
and project files before running Kit in a project. Invalid file handling does not change a valid subscription's runtime failure
policy: `fail-closed` can still prevent the corresponding operation.

## Supported boundaries and effects

| Events | Native authority |
| --- | --- |
| `session.start`, `session.end`, `turn.end`, `turn.progress`, `model.error` | Observation |
| `user.message.inbound`, `turn.start` | Replace the native prompt item array |
| `model.request.before` | Replace or merge inference-local request parameters |
| `model.response.after`, `turn.finish.before` | Replace editable, uncommitted native output items (normal finish only) |
| `tool.before` | Allow, deny, replace or merge tool input |
| `tool.after` | Replace or merge completed tool output |
| `context.compact.before`, `context.compact.after` | Replace summarizer instructions or the pre-installation summary |
| `model.switch.before`, `config.change.before` | Deny the actual ACP configuration operation |
| `model.switch.after`, `config.change.after`, `file.changed` | Observe committed native changes |

Interception is available only at the implemented native boundaries. SDK
content selection can narrow modification authority further: a hook that requests
only metadata cannot modify an unavailable body. Output-less tool receipts and
read-only terminal candidates advertise no output modification authority.

`turn.finish.before` fires exactly once per logical turn, but its authority
depends on how the turn ends. When a model response takes the normal finish
branch, the boundary runs immediately after `model.response.after` over the same
staged output, so both edits compose before anything commits. Every other ending
— cancellation partials, a tool-only round that ends the turn, an erased prompt,
a failed or retired turn — is delivered as an awaited observation with no
response target, so no edit can settle there. `turn.end` and `turn.progress` are
delivered on the loop's actual boundaries. `turn.progress` carries canonical
message parts for actual text/structured chunks; structural or incomplete binary
chunks carry a gap rather than a serialized native envelope. It also carries the
loop-authored placeholder result for a tool call that moved to the background.
A failed observation is recorded as a Kit diagnostic: it never fails the
operation it observed, never rolls back a commit, and never produces a second
terminal event.

`turn.start` runs once per logical turn the driver creates, before any inference
for it, with queued input already admitted. It therefore also covers synthetic
turns that carry background tool resolutions or a late approval, and turns whose
prompt another mutator emptied. Re-activating a tool round that outlived its
turn-finished event is a continuation, not a new turn, and does not run it again.

`model.error` reports only real model failures, classified by Kit at its own
adapter, session and turn boundaries: `provider` for a provider-reported
failure, `stream` for a stream that ended without a finish event, `adapter` for
an adapter-state failure, and `unsupported` for an adapter capability failure.
A tool error or a hook error can travel out of a model boundary, and neither is
reported there; cancellation is not a model error either and is observed through
the turn's terminal boundaries. For a provider failure the event carries the
retry layer's sanitized upstream classification as `error.code` and its source
HTTP status as `error.status` when the provider actually reported them — no
headers and no response body are retained, and an unclassified code is omitted
rather than sent as a placeholder. The `model` and `attempt` objects are Kit's
own session-selection snapshot and a session-local model-call index, not
provider routing or a provider retry number.

## Content projection and limits

This integration targets pinned SDK 0.4.1 and uses its public domain-named
inputs and canonical effective event types.

Kit's central SDK dispatch projects native AgentKit `Item` arrays at `/items`
and inbound items at `/message/messages` into canonical inline JSON message
lists. Each message has `id`, `role`, and `parts`. Text parts carry inline plain
text. `Structured`, `ToolCallInput`, and custom actual values carry JSON-text
payloads, not serialized native item wrappers. Hooks therefore edit the canonical
message representation rather than native AgentKit enum serialization.

Media and file parts are immutable. When Kit has their actual bytes, it binds
those bytes to invocation-owned SDK attachments; it does not attach the entire
prompt as a generic attachment. Unresolved native references and opaque multipart
tool results remain immutable gaps: dispatch does not implicitly fetch their
content or invent an editable text representation.

The published `session.end` schema is an exception: its read-only `/items` slot
is a flat part list rather than messages. Kit retains scoped part identities and
reports the unavailable per-message role field as a canonical gap. Native
transcript values remain unchanged.

The SDK-effective event is restored to native values before consumption, retaining
native metadata, accounting, and call identity. Native prompt and output validation
is unchanged: identities and tool linkage remain validated before commit, and
empty persistent prompt replacements are rejected. Model-request edits address the canonical `/items` messages and affect only
the current inference. Available tools, cache configuration, and request metadata
remain native context rather than editable prompt content. Tool output bodies retain their native `ToolOutput`
representation where supported by the boundary.

Dynamic capability grants are preserved through projection and restoration. Kit
uses the SDK 0.4.1 content contract directly, without legacy `content_target`
mappings. Content selection and the native boundary's authority still determine
which modifications are available; projection does not grant edits to immutable
parts or read-only terminal candidates.

Content and attachments belong to one hook invocation. Kit has no competing
content store. Kit explicitly configures `max_body_bytes` to 4 MiB,
`max_stored_bytes` to 64 MiB, and the attachment-owner limit to 4,096. Accounting
is shared across concurrent invocations. Kit also bounds inline text content at
4 MiB per native slot, before dispatch and effective writeback, without attachment
staging. The SDK owns binary content-limit handling,
transport deadlines, cancellation, failure policy, effect reduction, and shutdown.
Kit also explicitly configures the backend transport-frame ceiling to 4 MiB.
That ceiling covers the complete serialized protocol envelope, not an individual
content slot. These limits bound content capture and transport frames, not total
process RSS.
Selected content may be prepared before effects settle as denial; denial is not
an assurance that no attachment capture or transport preparation occurred.
Shared attachment reads use SDK 0.4.1's concurrency-safe implementation; Kit does
not impose a serial-read restriction. Effective content comes from the settled
SDK result, and invocation ownership is released on completion or failure.

Kit uses generated typed input facts and schema-owned `HostInput` builders for
owned binaries. Ordinary content stays inline. Specialized compaction
instructions and summaries use the SDK's typed `TextParts` lists, not an erased
message-list workaround. The retained registered-hook regressions require these
lists to settle as effective inline text under the configured fail-closed policy.
Named host boundaries return `EventOutcome<Event>` with the SDK's canonical
`settled_event`. Kit projects that typed event directly into native writeback;
it does not re-decode `effective_event` or construct a replacement SDK outcome.
Specialized tool settlement likewise provides a canonical `settled_event`,
which supplies the invocation's settled identity. The public complete-event
boundary remains necessary for standalone tool receipts, for which `Hooks` has
no named method. Low-level raw views remain available for intentional pointer
access and diagnostics, not as competing settlement authority. Canonical
settled events and complete-event raw inputs retain open event members;
named input facts expose the SDK's declared fields, including `native` and
`extensions`, rather than arbitrary top-level host fields. Opaque nested facts
and schema-supported open extras are retained without stripping unknown members. Native projection is still necessary to preserve Kit's private
metadata, accounting, immutable file owners, call identities, and transcript
lineage.

SDK consumer outcomes expose typed `MessageEffect`, complete `InjectEffect`,
and accepted `InterceptResponse` envelopes. Candidate ownership depends on the
originating event; application/tool values remain opaque unless the caller
supplies an input, result, or provenance contract. Kit's native tool arguments
remain JSON values. This migration does not advertise execution candidates or
injection delivery that Kit does not implement. Caller-type decoding and
contract failures are SDK failures, not permission to run a native operation;
accepted-settlement evidence is diagnostic and does not undo accepted effects
or grant execution. Denied native tools retain their effective input in the
skipped receipt, and delivery diagnostics remain visible.

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

These checks do not claim a live HerdR run against SDK 0.4.1. The separately
installed bridge remains built against SDK 0.4.0; final live smoke verification
is a separate delivery check. No external bridge repository or installed
version was changed.

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

Observation diagnostics, including deadline and cancellation entries recorded by
the SDK before notification delivery, are logged rather than dropped.

Kit does not advertise structured model/config modification targets,
hook-supplied tool results, approval prompts, context injection, or hook-requested
continuations. Unsupported host capabilities are not advertised. Events do not
invent provider retry facts, background completion, or changes that have not
committed.

Native cancellation interrupts in-flight admission, turn-start, model, progress,
tool and compaction boundaries through the SDK cancellation boundary. A boundary
the SDK cancels ends its turn as cancelled rather than failed, with no partial
items. Terminal boundaries — the read-only pre-finish, `turn.end` and
`session.end` — are delivered uncancelled, so a cancelled prompt still completes
its own cleanup: Kit signals cancellation through the loop's handle and then
awaits the in-flight driver call instead of abandoning it, which is what makes
those boundaries arrive exactly once over committed output that is neither lost
nor re-delivered. A hard abort (dropping the call, or the bounded server
shutdown path) remains best-effort: facts not yet delivered are lost and the
loop keeps no resumable delivery state. The SDK owns transport deadlines,
failure policies, content limits and shutdown.

## Session behavior

Each native driver owns a separate hook host. Durable prompts, ephemeral
prompts, ACP sessions, and native session forks use the same integration path.
Fork startup identifies the source session; resume startup identifies the resumed
session. The native permission checker is reported as `allow-all`; AHP policy
interception remains a separate boundary.

Closing one driver does not close a sibling driver's transports. Session closure
is Kit-owned: the owner cancels and awaits its own background and tool work,
retires any interrupted turn so that turn's terminal boundaries are delivered
first, then awaits `session.end` and SDK shutdown. `session.end` therefore
reports Kit's closure rather than a loop close, with an outcome of `completed`,
`cancelled`, or `error`. Every owner performs it — ordinary and ephemeral
prompts, ACP v1 and v2 sessions, forks, cancellation, failed admission, and
startup that opened a session before failing. Aborted drivers attempt transport
cleanup while the process runtime is still available.

Prompt admission is Kit-owned too. `user.message.inbound` covers the user's own
prompt items on every surface — durable prompts, ephemeral prompts, and ACP v1
and v2 prompts — through one admission path, and is dispatched after
`session.start` so a hook consumer still sees the session open before the prompt
it admits. A replacement then flows through the ordinary skill-catalogue and
voice pipeline into the driver. Kit's own submissions — the skill catalogue,
background-task notifications, compaction markers — are not inbound user
messages and are not dispatched there.

Cancelling an admission boundary ends that prompt as cancelled. A session whose
startup is cancelled before it hands over a driver still gets its `session.end`
and SDK shutdown; it has no transcript to report, which is stated as a gap on
`/items` rather than sent as an empty transcript.

Committed turn-start prompt rewrites are persisted as canonical transcript
replacements. Resuming or forking a durable session sees the rewritten prompt,
not the original admitted prompt. Ephemeral fork snapshots follow the same
replacement notifications, which the loop delivers as one canonical snapshot per
committed rewrite. Model-request-only edits remain inference-local.

A prompt replacement is staged and validated in full before any of it is
committed: an empty replacement, a duplicated item identity, a changed role or
changed accounting on an already committed item, a duplicated tool call, or an
orphaned tool result is rejected and nothing is written. A rejected or cancelled
replacement leaves the live transcript unchanged and delivers no notification.

Automatic compaction uses the same session-owned host at generation and
pre-installation boundaries.

Fresh native subagent subprocesses discover and merge global/project hook files.
Native ACP forks retain their source runtime's configuration. Programmatic
`Runtime::with_ahp` overrides are not currently forwarded to fresh subprocesses.

Top-level tools, Runlet child calls, and dynamic MCP calls use the native tool
executor bridge. Nested invocations retain their parent event lineage, including
calls moved into background compose work. A configured third-party ACP harness
is a separate process/runtime; this integration does not add native Kit hooks to
that harness.
