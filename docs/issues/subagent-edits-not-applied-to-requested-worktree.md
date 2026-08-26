# Subagent reported edits that were absent from the requested worktree

## Summary

A completed subagent reported that it edited `src/protocols/acp/v2.rs` in an explicitly named worktree and passed targeted checks. The parent session immediately inspected that worktree, but the file still matched `HEAD` and `git status` showed no modification for it. Re-prompting the same subagent to reapply the patch made the diff appear.

## Impact

The completion report and check results were not sufficient evidence that edits reached the requested worktree. The parent had to detect the missing diff and repeat the implementation step.

## Expected behavior

When a subagent is told to edit an absolute worktree path, its reported file changes and checks should apply to that path. If the harness uses an isolated fallback instead, the result should say so and return a patch or provide an explicit promotion step.
