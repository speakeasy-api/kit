# OpenAI subscription stream transport errors hide their cause

Two independent Kit processes failed at approximately the same time with only:

```text
loop error: provider error: openai-subscription stream transport failed
```

The shared timing points to a network or upstream stream interruption rather than session corruption, but the exact cause cannot be recovered. `OpenAiSubscriptionTurn::next_event` maps every `reqwest` body-stream error to the same static message and discards the source error. Provider failures shown by the TUI are not appended to the durable session transcript, and these processes had no telemetry endpoint configured. The remaining evidence cannot distinguish a connection reset, HTTP/2 stream failure, TLS interruption, body decoding failure, or another transport condition.

Kit should retain a safe, bounded classification of the underlying `reqwest::Error`, including timeout/connect/request/body/decode flags and a sanitized source-chain summary that excludes URLs, headers, credentials, and response bodies. The diagnostic should include a non-secret response or request identifier when available. Telemetry and stderr should receive the same classification even if the user-facing message stays concise.

A separate recovery improvement could retry once when the stream fails before Kit has emitted any model event, using the existing idempotency key and turn-state handling. Retrying after output has been observed requires stricter replay semantics and should not be conflated with better diagnostics.

Relevant implementation: `src/provider/chatgpt.rs` (`OpenAiSubscriptionTurn::next_event` and initial turn startup).
