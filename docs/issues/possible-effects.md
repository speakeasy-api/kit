# Possible-effects observations (issue #48)

A failed child prompt can have changed external state. Kit retains bounded,
provider-independent positive observations from ACP notifications, independently
of its output-retention limits and opt-in stderr display events. The owner of the
observations outlives the dispatched prompt task, so reply-channel loss does not
silently discard previously observed activity.

`possible_effects` contains only fixed field names, an allowlisted source, and
booleans. It never contains tool IDs, arguments, results, assistant text, prompts,
or provider payloads. `source: acp_notifications` identifies lifecycle **reports**
from a harness, not verified local execution receipts. A pending tool announcement
is not a start. A completed tool status does not imply that a start was observed. A failed
status can reflect denial or failure before execution, so it does not establish
completed execution.

All failed observations remain `observation_incomplete: true`. False means
**not observed**, not **did not happen**. Completion does not establish success,
a committed external effect, rollback, or replay safety. No automatic retry or
session reconstruction decision can be made from these facts alone.

Fatal records use schema version 3 with an additive `possible_effects` object.
Version 1/2 records remain readable; a missing object means unknown. Existing
root-session failure paths without an explicit observation owner also report
unknown, rather than claim complete coverage. Child failures record the child
prompt snapshot in a parent-session-scoped diagnostic with surface `subagent`.

## Current transport limitation

The pinned AgentKit `ToolError` exposes `ExecutionFailed(String)` and unit
`Cancelled`. The existing parent error behavior is unchanged. Effects remain
in a typed child error and the local fatal diagnostic; they are **not yet**
transported to the parent as native structured failure metadata. A rendered
JSON string is not a substitute for the shared typed error contract.

Do not return a completed `ToolResult` with `is_error: true` as a workaround:
the pinned Compose dispatcher treats completed child results as successful
values, bypassing Runlet error boundaries. Full #48 support still needs an
upstream typed error-metadata contract, including cancellation, plus root-session
and locally verified execution observation. Recovery (#21) must not infer that
an external harness can resume or fork an uncertain session from this metadata.
