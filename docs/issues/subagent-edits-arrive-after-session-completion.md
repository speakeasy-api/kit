# Subagent edits can appear after session completion

While using Kit 0.1.43, a completed subagent reported that it had not edited source files, but source changes from that session appeared in the parent working tree later. A newly created replacement file also disappeared and then reappeared with the subagent's version after the subagent had completed. Closing the subagent sessions stopped the conflicting updates.

This makes parent-side inspection and editing race with delayed subagent workspace synchronization. Completed sessions should finish synchronizing before returning, and read-only prompts should not publish source changes.
