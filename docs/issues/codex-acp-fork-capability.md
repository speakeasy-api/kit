# Codex ACP does not expose fork capability

The observed Codex ACP adapter does not advertise or implement ACP `session/fork`, so Kit rejects `fork` for Codex subagents. The adapter has exposed an underlying Codex `thread/fork` operation internally, but that operation is not surfaced through the ACP session capability and request.

This is principally an interoperability gap in the Codex ACP adapter. Kit should retain capability-driven behavior, and compatibility should be covered against supported adapter versions rather than inferred from private adapter internals.
