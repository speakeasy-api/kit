# Session transcript items omit timestamps

A post-session performance review could not derive phase durations directly from the persisted transcript because every JSONL item had `created_at: null`. Timing had to be reconstructed approximately from the timestamp embedded in the session ID, subagent IDs, runtime event payloads, and Git commit dates. Those clocks do not cover ordinary model/tool round trips consistently.

Persist a timestamp for every transcript item, ideally including separate dispatch and completion timestamps or elapsed duration for tool calls. Existing transcript loading should remain compatible with historical null timestamps. This would make slow harness launches, cancellations, model latency, tool execution, and integration work measurable without relying on external logs.

Observed in `.kit/sessions/<session>.jsonl`; relevant implementation is the session/transcript item persistence path.
