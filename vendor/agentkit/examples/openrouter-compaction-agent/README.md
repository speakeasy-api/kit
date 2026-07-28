# OpenRouter Compaction Agent

This example demonstrates three compaction modes using the real `agentkit` loop:

- `structural`: local pruning only
- `semantic`: semantic compaction through a nested loop that runs a dedicated compaction agent
- `hybrid`: local pruning plus nested-loop semantic summarization

The nested compaction path uses `agentkit-compaction::CompactionBackend` to run another agent to completion and convert older transcript spans into a durable context item.

## Usage

The example reads `OPENROUTER_API_KEY` and `OPENROUTER_MODEL` from the environment or `/.env`.

Run all modes:

```bash
cargo run -p openrouter-compaction-agent
```

Run one mode:

```bash
cargo run -p openrouter-compaction-agent -- --mode semantic
```

Override the final user prompt:

```bash
cargo run -p openrouter-compaction-agent -- --mode hybrid \
  "What is the sealed release codename? Return only the codename."
```

## Predictable Smoke Test

This is the simplest predictable check for the nested compaction path:

```bash
cargo run -p openrouter-compaction-agent -- --mode semantic
```

Expected behavior:

- compaction fires before the root turn
- the compaction backend runs a nested agent to summarize older transcript items
- the final assistant output contains `GOLDFINCH-17`
