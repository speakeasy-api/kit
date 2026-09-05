# Possible-effects observations (issue #48)

A failed prompt can have changed external state. Kit retains bounded,
provider-independent positive observations independently of output-retention
limits and opt-in stderr display events.

`possible_effects` contains only fixed field names, an allowlisted source, and
booleans. It never contains tool IDs, arguments, results, assistant text, prompts,
or provider payloads. Sources distinguish two observation scopes:

- `acp_notifications`: reports received during one dispatched child prompt.
  Pending tool announcements are not starts. A completed status does not imply
  that a start was observed. A failed status can reflect pre-execution denial,
  so it does not establish completed execution.
- `local_session`: cumulative observations during one **live root owner's
  lifetime**, shared by its loop observer, compose, and hidden tool wrappers.
  These are not facts attributable exclusively to the failing prompt, nor do
  they cover earlier process instances or a resumed session's stored history.
  Execution starts and completions come from actual invocation entry/terminal
  return boundaries, not synthesized tool-result events. Entry into a local
  invocation does not establish dispatch or execution of an external operation.
- `unknown`: no explicit observation source is available.

Local assistant-output tracking excludes reasoning and tool-argument deltas,
handles committed content without prior deltas, and bounds transient part
classification by identifier length and entry count. Presentation supersession
and new prompts clear only transient classification, never positive evidence.
Detached work retains the same owner when it spans prompts.

All failure snapshots remain `observation_incomplete: true`. False means
**not observed**, not **did not happen**. A completion boolean means some
invocation completed; it does not establish that every invocation or external
operation completed. Interruptions and dropped futures are not completions.
Completion establishes neither successful external effects, rollback, nor replay
safety. No automatic retry or reconstruction decision follows from these facts.

Child observation ownership survives dispatched-task/channel loss and
post-response ownership rejection. Cleanup failures preserve the original
failure classification. Root finalization may return a cleanup or flush error instead
of the original driver error; this gets a separate `session_finalization`
diagnostic without replacing the earlier driver record. Root prompt, ACP v1/v2, and A2A failure paths retain
explicit owners, including unstructured and unsolicited execution. Cancellation
keeps its existing lifecycle classification and records a snapshot after
structured cleanup when available; Kit does not wait for unrelated detached work
merely to improve evidence.

Fatal writers use schema version 4, adding the `local_session` source to version
3's effects object. New readers retain v1/v2 missing-effects defaults and v3
`unknown`/`acp_notifications` records; existing supplied effects are preserved.
Malformed fields, false completeness claims, and unknown future source values are rejected. This backward
read compatibility does not mean an older strict reader understands a new
source value. Files are not rewritten on read. Parent-side observation records
are distinct diagnostics, not recreated child fatal receipt identities.

## Remaining typed transport and release gates

The pinned AgentKit `ToolError` exposes string execution failures and unit
cancellation. The existing parent error behavior remains unchanged. The shared
upstream API implementation is authorized and reliability-owned; Kit integration
waits for reviewed compatible published crates rather than Cargo patches.

Required integration is typed, validated metadata through ACP error data, child
failures, subagent tools, Runlet's documented catch/rethrow policy, and both
foreground/background terminal projections, retaining one child fatal receipt
with explicit storage disposition. Cancellation must keep its classification.
A rendered JSON string is not a substitute for that contract.

Do not return a completed `ToolResult` with `is_error: true` as a workaround:
the pinned Compose dispatcher treats completed child results as successful
values, bypassing Runlet error boundaries. Recovery (#21) remains gated until
complete #48 delivery and review; effects alone cannot reconstruct a session.
