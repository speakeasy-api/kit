# Pinned ACP v2 schema

`acp-v2.schema.json` and `acp-v2.meta.json` are copied verbatim from the ACP
unstable v2 schema at `danielkov/agent-client-protocol` commit
`b7ddb8370e72e3adb6b895f879876cdd3f717ab9`, the source of the `agentkit-acp-schema` 1.9.1 crate in
Kit's Rust dependency graph. They are persistent, versioned build inputs.

Run `scripts/generate-acp-swift.py` after intentionally updating the pin. CI runs
`scripts/generate-acp-swift.py --check`; generation is local and never fetches
from the network. Existing pins are not migrated or rewritten at runtime.
