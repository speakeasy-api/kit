# Headless ACP subagents cannot request permission

Kit answers every nested ACP `RequestPermissionRequest` with `Cancelled` so a headless child cannot wait indefinitely for a human response. As a result, ACP harnesses that require approval for protected actions must be configured in a noninteractive or pretrusted mode, or those actions fail.

A future design could route permission requests through the parent interaction surface while preserving cancellation, attribution, and noninteractive safety. Until then this limitation should remain explicit in harness setup guidance.

Relevant implementation: `src/acp_child.rs` request handler.

## Current status

Profiles now have a fail-closed `permissions = "deny" | "cancel"` policy. `deny` selects only ACP rejection options and never approval; `cancel` preserves the previous behavior. Interactive routing to a parent user remains unresolved.
