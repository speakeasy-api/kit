# Subagent prompt invalidated by unknown Responses SSE `keepalive`

While using Kit as the agent harness, an internal ACP subagent `prompt` tool call failed when the provider stream emitted an unknown Responses SSE `keepalive` event. The tool failure also invalidated that harness-owned subagent session, so the implementation task had to be restarted in a new session.

Kit's ACP harness transport should tolerate provider keepalive events (or otherwise classify them as non-terminal transport metadata) without losing an otherwise healthy subagent session.

## Resolution

Responses SSE `keepalive` frames are now ignored as transport metadata while unknown event kinds still fail. A regression test covers both behaviors.
