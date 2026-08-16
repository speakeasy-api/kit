# Cursor ACP does not expose fork capability

The observed Cursor Agent ACP implementation does not advertise ACP `session/fork`. Kit consequently cannot branch Cursor-backed subagents and returns the generic unsupported-fork error.

This is principally an interoperability gap in the Cursor ACP implementation. Compatibility should be rechecked as Cursor's ACP capabilities evolve.
