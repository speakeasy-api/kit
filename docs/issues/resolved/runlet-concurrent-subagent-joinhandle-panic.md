# ACP startup failure double-polled its `JoinHandle`

A Runlet concurrency check launched five independent `acp.kit` subagents from a `for` expression. Instead of returning a structured startup error, `compose` failed with an internal panic:

```text
compose runlet execution panicked: task 91 panicked with message "JoinHandle polled after completion"
```

The defect was in `ChildSession::start`, not Runlet's concurrent iteration. Its startup `tokio::select!` polled the ACP actor's `JoinHandle` when the actor exited before sending `Ready`. That branch stored an error in the common result, after which common error cleanup aborted and awaited the same handle. Tokio join output is single-consumption, so the second poll panicked.

The race was exposed when both the dropped readiness channel and completed actor were ready; multiple concurrent startups made it easier to hit. The panic also masked the original subprocess, transport, initialization, or new-session error.

Resolved in `ed05858` by returning directly from the actor-completion branch so cleanup cannot poll an already-consumed handle. Cancellation, timeout, and readiness-channel failure retain abort-and-await cleanup because those branches have not consumed the handle. A concurrent regression test starts 64 profiles with a guaranteed-missing executable and verifies that every outer Tokio task joins without panic while every startup returns `ChildError::Failed`.
