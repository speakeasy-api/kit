# ACP subagent output capture is text-only

The nested ACP client records only text `AgentMessageChunk` notifications and concatenates them into the subagent tool's `output` string. Other ACP updates—including thoughts, plans, tool calls, images, and non-text content blocks—are ignored.

This keeps the model-visible return value small, but loses useful execution context and prevents faithful representation of agents that primarily emit richer ACP content. The desired structured output and event-forwarding behavior needs to be defined before expanding the tool schema.

Relevant implementation: `src/acp_child.rs` notification routing.

## Resolution

Subagent values now optionally expose bounded `updates` containing non-text agent-message content, tool calls and updates, and plans. Text-only responses retain the original JSON shape. Thoughts, usage, user echoes, and session metadata remain intentionally excluded, and count/byte limits report truncation.
