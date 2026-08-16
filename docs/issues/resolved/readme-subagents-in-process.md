# README incorrectly describes subagents as in-process

The README's "Deliberate limits" section says subagents are in-process. Reusable subagents are now parent-owned ACP subprocesses, although they share the configured root and remain bounded by the nesting-depth limit.

Update the statement to reflect the subprocess architecture and its actual isolation boundaries. The stale wording predates the reusable ACP subprocess implementation.

## Resolution

The README now describes subagents as parent-owned ACP child processes sharing the configured working root and bounded to depth two.
