# Pinned ACP v2 schema

`acp-v2.schema.json` and `acp-v2.meta.json` are copied verbatim from
`schema/v2/schema.unstable.json` and `schema/v2/meta.unstable.json` in the
prompt-delivery schema worktree (`../acp-schema-prompt-delivery-main`, based on
upstream ACP commit `423e2a726fdad793bc9efe87b0c7182b8ff602ca`). They are
persistent, versioned build inputs.

Run `scripts/generate-acp-swift.py` after intentionally updating the pin. CI runs
`scripts/generate-acp-swift.py --check`; generation is local and never fetches
from the network. Existing pins are not migrated or rewritten at runtime.
