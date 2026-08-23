# OpenAI subscription stream transport errors hide their cause

Two independent Kit processes failed at approximately the same time with only:

```text
loop error: provider error: openai-subscription stream transport failed
```

The shared timing points to a network or upstream stream interruption rather than session corruption, but the exact cause cannot be recovered. `OpenAiSubscriptionTurn::next_event` maps every `reqwest` body-stream error to the same static message and discards the source error. Provider failures shown by the TUI are not appended to the durable session transcript, and these processes had no telemetry endpoint configured. The remaining evidence cannot distinguish a connection reset, HTTP/2 stream failure, TLS interruption, body decoding failure, or another transport condition.

Kit should retain a safe, bounded classification of the underlying `reqwest::Error`, including timeout/connect/request/body/decode flags and a sanitized source-chain summary that excludes URLs, headers, credentials, and response bodies. The diagnostic should include a non-secret response or request identifier when available. Telemetry and stderr should receive the same classification even if the user-facing message stays concise.

A separate recovery improvement could retry once when the stream fails before Kit has emitted any model event, using the existing idempotency key and turn-state handling. Retrying after output has been observed requires stricter replay semantics and should not be conflated with better diagnostics.

## Resolution

Kit now preserves bounded transport classifications (`timeout`, `connect`, `request`, `body`, and `decode`) without serializing the `reqwest::Error` or its potentially sensitive URL. OpenAI subscription turns retry transient request, HTTP, provider-event, idle-timeout, early-close, and stream-transport failures only before the first model event. All attempts reuse the idempotency key and are bounded by 25 retries and a 10-minute wall-clock budget.

Terminal top-level failures are also recorded under `~/.kit/errors/<session-id>/` with bounded messages, URL redaction, owner-only Unix permissions, atomic creation, and per-session retention. Cancellation does not produce a fatal record.

Relevant implementation: `src/provider/chatgpt.rs` (`OpenAiSubscriptionTurn::next_event` and initial turn startup), `src/fatal.rs`, `src/protocols/acp.rs`, and `src/runtime.rs`.
