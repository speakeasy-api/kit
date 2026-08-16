# Generic ACP subagents have no resume-based branch fallback

Kit does not use generic session persistence, load, or resume facilities to reconstruct or branch a non-Kit ACP subagent when native `session/fork` is unavailable. Generic sessions are reusable only while their parent Kit process remains alive; persistence after that is agent-defined.

A fallback cannot be added safely until the required snapshot and resume semantics are explicit. Reusing a mutable session or replaying prompts is not necessarily equivalent to forking a completed state.

Relevant documentation: `README.md`, "Reusable subagents".
