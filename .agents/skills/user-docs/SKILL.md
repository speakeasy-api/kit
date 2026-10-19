---
name: user-docs
description: Use when writing, editing, or reviewing customer-facing documentation, including docs/user pages, setup guides, feature instructions, and troubleshooting guidance.
---

# Problem-focused user documentation

## Before editing

- Identify the reader's problem and the action or decision the page must support. Assume the reader may be an LLM retrieving this page to solve a specific problem, without development history or surrounding conversation.
- Read the existing page and relevant linked guidance. Check current behavior in source or CLI help when changing factual claims. Do not infer new behavior from a prose cleanup.

## Write for the task

1. Lead with the problem or desired outcome, then give the shortest useful path to the solution: prerequisites, concrete steps or a runnable example, expected result, and relevant failure recovery.
2. Use descriptive task headings and explicit product, command, setting, and error names so a retrieved section stands on its own. Link to related tasks instead of duplicating their instructions. Preserve existing anchors or update links when renaming headings.
3. Keep technical details that affect a reader's choice, action, safety, cost, compatibility, or troubleshooting. Exact schemas and protocol rules belong when the reader must implement that interface. Do not turn useful reference tables into vague prose or force every section into a literal problem/solution template.
4. Remove implementation inventories, internal execution sequences, storage bookkeeping, development issue references, and descriptions of how a patch was built. Translate a mechanism into its observable consequence when that consequence matters. Move maintainer-only material to an appropriate developer reference only when it has a concrete audience and use.
5. Describe the current supported behavior, not the chronology of its implementation. Avoid diary or changelog framing such as "now", "new", "previously", or "the existing form" unless a specific migration task requires a version distinction and an action.
6. Put limitations beside the affected action. For troubleshooting, connect a recognizable symptom to a cause and a remedy. Retain warnings and recovery constraints; do not erase important caveats merely to shorten a page.
7. Keep scope focused. Do not add speculative advice, unsupported guarantees, promotional claims, or exhaustive lists of internals. Prefer one canonical explanation for each task.

## Example of the distinction

For resuming delegated work, explain which parent session to reopen, how to find and continue a child, whether interrupted work retries, and what to do if recovery fails. The names of stored transcript fields and the reconnect handshake do not help that reader. An adapter author may need the handshake in a separate protocol reference.

## Review before finishing

- For each paragraph, ask: what can the reader decide or do with this information? Remove or relocate material with no answer.
- Check that examples, defaults, limits, warnings, and interface contracts remain accurate. Review deletions as carefully as additions.
- Check local links and changed heading anchors, code fences, and repository documentation conventions. Run the smallest relevant checks.
- For a broad audit, account for every requested page, including pages that need no changes. Report coverage and unresolved factual questions without adding the audit diary to user docs.
