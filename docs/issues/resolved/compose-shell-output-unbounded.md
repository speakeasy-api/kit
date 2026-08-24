# Compose shell output was unbounded in model context

Resolved in Kit 0.1.62 and corrected in Kit 0.1.79.

Kit originally bounded shell `stdout` and `stderr` before returning them to compose. That protected model context, but it also gave Runlet and downstream tools the truncated preview. Kit 0.1.79 moves the 8 KiB spill guard to the final compose boundary: hidden shell results remain complete for in-program consumers, while oversized final results are stored under `~/.kit/artifacts` and the model receives a bounded head-and-tail preview plus the artifact path. Shell streams and final compose results retain separate 64 MiB safety limits rather than silently truncating internal values. Artifact files use owner-only permissions on Unix. The system prompt also asks agents to use targeted, bounded commands and avoid credential or environment dumps.

Parent-visible ACP subagent updates now remove rendered `content` when it is only a JSON copy of the update's structured `rawOutput`, preserving distinct human-readable content.
