# OpenAI Responses top-level `error` SSE event was misclassified

An OpenAI subscription turn in persisted session
`s-1786991996634-21884-4` failed after generation 344 with:

```text
openai-subscription protocol error: unknown Responses SSE event: error
```

Kit recognized `response.failed`, but not the Responses protocol's top-level
`error` event. The catch-all therefore reported a client protocol incompatibility
and discarded the provider's error classification.

Resolved in Kit 0.1.31. Top-level `error` and `response.failed` events now share
bounded, sanitized transient/auth/permanent classification. Retriable failures
are retried once, honoring bounded `retry_after`, only when they arrive before
any model output is observed. The retry reuses the request's idempotency key and
shares the existing HTTP retry budget. Once output is observed, queued output is
delivered before the terminal failure and the request is not redispatched; this
avoids duplicate streamed output when dispatch status is ambiguous. Regression tests cover classification, same-chunk event
ordering, terminal-frame handling, and cancellation ahead of prefetched output.
