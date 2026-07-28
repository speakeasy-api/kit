# OpenRouter ACP Trio

Interactive ACP multi-agent coding example.

On start, the example creates a fresh JavaScript calculator project under your
temp directory. The project intentionally starts small and partly broken:

- `src/calculator.js` exports `add`, `subtract`, and `divide`.
- `subtract` contains a real bug.
- `test.js` is a Node/assert test file the agents can inspect or update.

The binary then starts three OpenRouter-backed agentkit agents and exposes each
one as an in-memory ACP endpoint:

- `orchestrator`: receives user prompts, can only read files and call the
  other agents over ACP. It cannot edit files.
- `worker`: can edit files in the scratch project and can ask the reviewer for
  feedback over ACP.
- `reviewer`: reads the project and either calls the worker over ACP for fixes
  or returns an `APPROVED:` signal to the orchestrator.

Each agent gets ACP tools for the other two agents. Agent-to-agent tool calls
use fresh ACP sessions against the target endpoint, while the user-facing
orchestrator REPL keeps a persistent session for follow-ups.

The REPL first offers three concrete coding tasks:

1. Fix the subtraction bug.
2. Add multiplication.
3. Harden division by zero.

After the first task, the same orchestrator session stays alive and accepts
follow-ups until `/q`, `/quit`, or `/exit`.

Every ACP handoff and loop event is logged with prefixes such as `[ORCH]`,
`[WORKER]`, and `[REVIEW]`, so you can see which agent read, delegated, edited,
reviewed, or approved each step.

Run:

```sh
OPENROUTER_API_KEY=sk-or-v1-... cargo run -p openrouter-acp-trio
```

Optional:

```sh
OPENROUTER_MODEL=anthropic/claude-sonnet-4.5 cargo run -p openrouter-acp-trio
```

Pick one of the three initial tasks, then keep sending follow-ups until `/q`.
The scratch project path is printed at startup and left on disk when the REPL
exits, so you can inspect the final files and run `node test.js` yourself.
