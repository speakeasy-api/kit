---
name: updating-artifact-schema
description: Use whenever changing the schema of a persistent artifact, including config files, transcripts, caches, checkpoints, credential metadata, or other data read across software versions.
---

# Updating Artifact Schemas

1. Inspect every reader, writer, strict parser, test, and historical shape before editing.
2. For hand-written artifacts, prefer ordered, shape-driven, idempotent migrations without a required version key. Use explicit versions only when shapes cannot be distinguished safely.
3. Parse into a generic representation, run all migrations in order, then strictly deserialize the latest shape.
4. Add one migration per schema change; never rewrite an older migration. Preserve the new value when old and new fields coexist.
5. Make every writer emit only the latest shape. Do not rewrite user-owned files on read unless explicitly required.
6. Test legacy and current shapes, mixed-key conflicts, malformed values, and writer output.
7. Follow repository release rules, then run the smallest relevant format, test, and lint checks.
