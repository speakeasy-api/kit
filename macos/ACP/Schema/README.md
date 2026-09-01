# Pinned ACP v2 schema

`acp-v2.schema.json` and `acp-v2.meta.json` are copied verbatim from the ACP
unstable v2 schema at `agent-client-protocol` commit
`6e7e044f9464c4fd652d90699a09e9edc8b3bbad`, the same revision pinned through
Kit's Rust dependency graph. They are persistent, versioned build inputs.

Run `scripts/generate-acp-swift.py` after intentionally updating the pin. CI runs
`scripts/generate-acp-swift.py --check`; generation is local and never fetches
from the network. Existing pins are not migrated or rewritten at runtime.
