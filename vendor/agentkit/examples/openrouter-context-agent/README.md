# `openrouter-context-agent`

One-shot OpenRouter example that wires together:

- `agentkit-loop`
- `agentkit-provider-openrouter`
- `agentkit-context` — loads `AGENTS.md` eagerly
- `agentkit-tool-skills` — discovers skills progressively via the `activate_skill` tool
- `agentkit-reporting`

It accepts a single prompt argument, loads `AGENTS.md` into context, registers discovered skills as a tool, runs one turn to completion, and exits. Skills are **not** loaded eagerly — the model sees a catalog of skill names and descriptions in the tool description and activates them on demand.

## Run

Basic smoke test — the model answers from AGENTS.md context alone:

```bash
cargo run -p openrouter-context-agent -- \
  --context-root examples/openrouter-context-agent/fixtures/project \
  "What deployment environment does the project instructions prefer? Return only the environment name."
```

Test progressive skill activation — the answer requires reading the deploy-checks skill (the pre-deploy checklist is not in AGENTS.md):

```bash
cargo run -p openrouter-context-agent -- \
  --context-root examples/openrouter-context-agent/fixtures/project \
  "What are the 5 pre-deploy checks for the lantern project? List them as a numbered list."
```

Test the release-notes skill — the release note format and rules are only in the skill:

```bash
cargo run -p openrouter-context-agent -- \
  --context-root examples/openrouter-context-agent/fixtures/project \
  "Write a release note entry for lantern v0.4.0 released today. Added: dark mode support (#201). Fixed: memory leak in cache layer (#198)."
```

## Notes

- The example loads environment variables from the workspace `.env`.
- If `--context-root` is omitted, the current working directory is used.
- `AGENTS.md` is discovered by searching the context root and its ancestors.
- Skills are discovered from `<context-root>/skills` and `<context-root>/.agents/skills`.
- If no skills are found, the `activate_skill` tool is not registered.
