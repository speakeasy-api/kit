# DR-0008: Hunk-anchored edit format

- Status: accepted in principle (git-decoupling directive, 2026-08-09)
- Problem: kit_edit operations pin `expected_revision` plus a byte-range and
  `base_digest` per operation. That binds edits to workspace revision epochs
  and digest bookkeeping that exist only to detect stale reads — machinery the
  git-decoupling decision declared no longer useful. Parallel human/agent
  edits invalidate whole-file digests even when the edited region is
  untouched.

## Decision

kit_edit accepts codex-style hunks. An operation is
`{path, hunks: [{context_before, old, new, context_after}]}` over UTF-8 text
lines. Apply resolves each hunk by finding the unique occurrence of
`context_before + old + context_after` in the current file content:

- exactly one match → replace `old` with `new` in place;
- zero matches → typed failure `edit_anchor_not_found` (the model's view is
  outdated — re-read the file);
- multiple matches → typed failure `edit_anchor_ambiguous` (add context).

No `expected_revision`, no per-operation `base_digest`, no byte ranges. The
file content itself is the concurrency token: an outdated view cannot anchor.

## Mechanism

- New edit IR in src/workspace/edit: hunk resolution against the live file,
  producing the same internal replacement plan the staging pipeline already
  consumes (syntax validation and atomic materialization stay).
- Insertions: empty `old` with non-empty context; deletions: empty `new`.
  New-file creation keeps an explicit `create` operation.
- Matching is exact on line content after newline normalization; no fuzzy
  matching in v1.
- Tool schema changes → native schema digest changes → rides DR-0007
  supersede; ext-m006 pin updated.

## Not in scope

- Fuzzy/whitespace-tolerant anchoring, rename/move operations, binary files.
