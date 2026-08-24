# Subagent edits can appear after session completion

While using Kit 0.1.43, a completed subagent reported that it had not edited source files, but source changes from that session appeared in the parent working tree later. A newly created replacement file also disappeared and then reappeared with the subagent's version after the subagent had completed. Closing the subagent sessions stopped the conflicting updates.

The report attributed this to delayed subagent workspace synchronization. Kit has no such synchronization or publish step: ACP subagents run directly against the shared runtime root, so their writes are visible when they occur.

## Resolution

Closed as a misdiagnosis. Kit 0.1.43 already supported detached background compose calls, which intentionally can remain active after the foreground turn completes. Closing an ACP session cancels its running task-manager work, so the observation that closing the sessions stopped later changes is consistent with background work rather than delayed workspace synchronization.

No Kit mechanism has been identified that could buffer a completed subagent's file changes and publish them later. A future reproduction without running background work should be recorded as a new issue with the relevant transcript and tool-call IDs.
