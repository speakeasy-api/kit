# ACP subagent can fail structured output without a recoverable response

Re-prompting a completed `acp.codex` review session with an `output_schema` failed because the subagent returned non-JSON text. The `prompt` tool surfaced only an invalid-JSON error and did not return an updated reusable session value, so the review could not be continued or repaired in the same session.

The harness should either enforce the requested schema before completing, preserve and return the session on schema-validation failure, or expose the raw response so the parent can recover without discarding the review context.

## Resolution

Completed turns with invalid JSON or schema-mismatched output now return the raw text and advance the reusable session generation. Transport and prompt failures still retire the session.
