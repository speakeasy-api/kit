# Subagent harness overrides were underspecified

Status: resolved in Kit 0.1.25.

The model-visible `subagent` schema exposed an optional `harness` enum but did not
explain that omission deliberately selects the configured default. This made an
explicit harness look like ordinary model selection and encouraged unnecessary
overrides of user configuration.

Kit now states the semantics in the runtime system prompt, tool description, and
`harness` input-field description: supplying `harness` overrides the user's configured preference with that value.
Agents should default to omitting it.
