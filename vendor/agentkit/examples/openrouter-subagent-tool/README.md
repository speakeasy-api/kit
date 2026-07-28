# `openrouter-subagent-tool`

One-shot OpenRouter example that proves custom tools can run nested agents.

The root agent gets a `Subagent` tool. The root agent does **not** know the sealed launch code. The custom tool starts a second loop with private context that includes the code, runs that nested loop to completion, and returns the sub-agent output back to the root agent.

## Run

```bash
cargo run -p openrouter-subagent-tool -- \
  "Retrieve the sealed launch code via the Subagent tool and return only the code."
```

By default the private code is `LANTERN-SECRET-93B7`. You can override it:

```bash
SUBAGENT_SECRET="OWL-SECRET-1182" cargo run -p openrouter-subagent-tool -- \
  "Retrieve the sealed launch code via the Subagent tool and return only the code."
```

## Prove It With A Live Test

This package also includes an ignored live integration test that asserts:

- the root agent called `Subagent`
- the final root-agent output contains the secret

Run it with:

```bash
cargo test -p openrouter-subagent-tool root_agent_retrieves_secret_via_subagent -- --ignored --nocapture
```
