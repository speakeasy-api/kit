# Pinned ACP v2 schema

`acp-v2.schema.json` and `acp-v2.meta.json` are copied verbatim from
`schema/v2/schema.unstable.json` and `schema/v2/meta.unstable.json` in the
[`agentkit-acp-schema` 1.19.0 source commit](https://github.com/danielkov/agent-client-protocol/tree/a20b71197e7f948271913804b760e11669f89e78).
They are persistent, versioned build inputs.

Run `scripts/generate-acp-swift.py` after intentionally updating the pin. CI runs
`scripts/generate-acp-swift.py --check`; generation is local and never fetches
from the network. Existing pins are not migrated or rewritten at runtime.
