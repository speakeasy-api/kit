# Compose shell output was unbounded in model context

Resolved in Kit 0.1.62.

Shell `stdout` and `stderr` now remain inline only through 8 KiB. Larger streams spill in full to per-session, per-call files under `~/.kit/artifacts`, while the model-visible result contains a bounded head-and-tail preview and artifact path. Artifact files use owner-only permissions on Unix. The system prompt also asks agents to use targeted, bounded commands and avoid credential or environment dumps.

Parent-visible ACP subagent updates now remove rendered `content` when it is only a JSON copy of the update's structured `rawOutput`, preserving distinct human-readable content.
