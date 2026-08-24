# Resolved: OpenAI subscription stream body errors were not retried

Status: resolved in Kit 0.1.84.

An ACP session on Kit 0.1.82 terminated with:

```text
openai-subscription stream transport failed (timeout=false, connect=false, request=false, body=false, decode=true)
```

Kit treats `reqwest::Error::is_decode()` as evidence that a stream failure is non-transient. That classification is not valid for `Response::bytes_stream()`: reqwest 0.13.4 wraps every underlying HTTP response-body frame error with its internal `decode` error kind. A connection reset, HTTP/2 stream failure, or similar transient body transport interruption therefore reports `decode=true`, and `retriable_transport_error` rejects it before considering recovery. The early-stream retry path consequently returns a fatal provider error instead of retrying the request with the existing idempotency key.

The fatal record intentionally retains only boolean reqwest classifications, so this incident's underlying hyper or rustls source cannot be recovered. The durable session ends after a tool result and has no later model item; this is consistent with the provider stream failing while the next model response was being read, but does not identify the lower-level network cause.

Recovery should classify response-stream errors according to the operation context instead of using `!error.is_decode()`. Before the first emitted model event, a `bytes_stream()` error should be eligible for the existing bounded idempotent retry path. If malformed content decoding must remain non-retriable in other call sites, it needs a separate stage-aware classification or a bounded sanitized source-chain classifier. Add a regression test using a real reqwest body-stream error, because the current stream-close test does not exercise reqwest's `Decode` wrapper.

Relevant implementation: `src/provider/chatgpt.rs` (`OpenAiSubscriptionTurn::next_event`, `retriable_transport_error`, and initial turn startup) and reqwest 0.13.4 `src/async_impl/response.rs` (`Response::bytes_stream`).

## Resolution

Stream-stage reqwest errors, including `Decode`-wrapped body failures, now use the existing bounded idempotent retry before Kit emits the first model event. Request-stage classification remains conservative. A regression test makes the first HTTP response fail mid-stream, verifies Kit sends a second request, and confirms both attempts use the same `Idempotency-Key`; a separate test covers the post-event no-retry boundary.

Fatal schema v2 also records bounded structured transport classifications. The diagnostics move through the provider error in a strict internal suffix and are parsed fail-closed by fatal logging. The durable record contains only allowlisted reqwest, Hyper, HTTP/2, I/O, stage, retry, attempt, truncation/unknown fields, and a strictly validated provider `x-request-id` for support correlation; it does not retain arbitrary headers, raw source text, or response content. Schema v1 records remain readable.
