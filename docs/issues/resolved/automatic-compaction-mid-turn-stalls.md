# Automatic compaction can stall a turn after a tool result

When automatic compaction ran between tool rounds, the compaction summary could be placed after the latest user item. The agent loop treats a trailing developer item as passive transcript state, so it completed the active turn without another model request. The TUI returned to the prompt and the user had to send `continue`, even though the original turn was unfinished. A background notification arriving immediately after compaction could mask the bug by becoming new pending input.

Relevant implementation: `src/compaction.rs` and the agent loop's trailing-input check after `MutationPoint::AfterToolResult`.

## Resolution

Automatic compaction now follows OpenCode's checkpoint pattern: it summarizes an older prefix, folds in the previous checkpoint, and retains a token-targeted, tool-safe recent tail. A mid-turn replacement therefore keeps the completed tool round as active input instead of ending in a passive summary. Oversized recent tool outputs are truncated without losing call/result identity, and stale pre-compaction usage is removed so the checkpoint does not immediately retrigger. Regression tests cover mid-turn tool-pair retention, output bounds, and command-only manual compaction.
