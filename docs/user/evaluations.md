# Evaluate content with named questions

Use `eval` to classify content, rate it against a rubric, or estimate whether a
condition holds. One call evaluates multiple independent questions against the
same supplied state and returns named answers.

## Enable evaluations

Evaluations are off by default. Enable them in your user configuration only:

```sh
kit config set experimental.eval true
kit config set credential_store keychain
kit auth login typesafe
```

These settings persist in `~/.kit/config.toml`: evaluations are enabled and
login and subsequent Kit runs use the same persistent `keychain` credential
store. Do not override that store when starting Kit. See
[TypeSafe authentication](getting-started-and-configuration.md#typesafe-api-key)
for the API key prerequisite and the file-store alternative; if using that
alternative, persist both `credential_store` and `credential_dir` with
`kit config set` so login and subsequent runs share the same store. A project's
configuration cannot enable evaluations. You can use `TYPESAFE_API_KEY` instead
of a saved key; a nonempty environment value takes precedence. Restart Kit after
enabling the feature or adding a key. Without both opt-in and an available key,
`eval` is not offered to the agent.

**Each evaluation sends the supplied state and questions to TypeSafe and uses
your TypeSafe quota.** Supply only content you intend to share. Kit does not
choose content automatically, compact conversations, or filter other tool
results through evaluations. Evaluations introduce no content logging or
telemetry. As with other tool calls, inputs and results are part of the session.

Disable with `kit config set experimental.eval false` and restart Kit.

## Ask several questions in one call

`eval` is a hidden Compose tool, not a separate Runlet expression:

```text
return eval({
  state: "My invoice was charged twice. Please help.",
  questions: {
    urgent: { type: "noul", instructions: "Does this require immediate attention?" },
    department: {
      type: "choice",
      instructions: "Which department should handle this?",
      criteria: { billing: "Payments and invoices", support: "Product help" }
    },
    frustration: {
      type: "score",
      instructions: "How frustrated is the customer?",
      criteria: ["Calm", "Frustrated", "Very angry"]
    }
  }
})
```

- **Noul** returns `noul`, the probability of yes from 0 to 1. Optional `criteria`
  can describe `true` and `false`.
- **Choice** returns `choice`, option `probabilities`, and `confidence`. Supply
  1–255 named options; a description can be `null`.
- **Score** returns `score`, level `probabilities`, `legend`, and `confidence`.
  Supply 2–10 ordered levels. Levels start at zero; scores can fall between them.

State and instructions accept text, objects, or arrays. Each result includes the
resolved Jev model version and input/output token usage. The selected model is
`jev-latest`. Answers are probabilistic assessments, not guarantees. This tool
does not generate arbitrary JSON schemas.

A call accepts up to 64 questions and 256 KiB of encoded input. Results are
limited to 1 MiB. Up to four evaluations run concurrently per runtime; each
call has a 30-second deadline, including time waiting to start. Kit sends one
request per call, does not split or batch calls, and never automatically retries.
Cancellation stops waiting but cannot undo an evaluation already submitted;
a cancelled, timed-out, or failed call may still use quota.

See the [TypeSafe API reference](https://docs.typesafe.ai/api.md) for the question
and answer definitions.
