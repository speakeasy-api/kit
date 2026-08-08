# Native OpenAI Subscription Compatibility

Kit implements browser OAuth and the ChatGPT subscription Responses transport directly. It does
not invoke or read files from OpenAI Codex or OpenCode. The OAuth request, token refresh/revoke
forms, Responses wire items, and SSE lifecycle are derived from these commit-addressed sources:

- OpenAI Codex login and SSE handling (Apache-2.0) at commit `79b4f03d35962b005b007a015113b38930711665`, specifically `codex-rs/core/src/client.rs`, `codex-rs/login/src/server.rs`, `codex-rs/login/src/pkce.rs`, `codex-rs/login/src/auth/revoke.rs`, `codex-rs/login/src/token_data.rs`, and `codex-rs/protocol/src/models.rs`:
  https://github.com/openai/codex/tree/79b4f03d35962b005b007a015113b38930711665/codex-rs
- OpenCode native OpenAI integration (MIT) at commit `fe82a1b6ca4f535beb973b0867017e3f639f85ed`:
  https://github.com/anomalyco/opencode/blob/fe82a1b6ca4f535beb973b0867017e3f639f85ed/packages/opencode/src/plugin/openai/codex.ts

The required copyright notices and full license texts are distributed in
`THIRD_PARTY_NOTICES.md` and `third_party/licenses/`.

`openai-subscription` is separate from Kit's API-key `openai` provider. It uses the public native
client ID, registered loopback callbacks, OS credential storage, and
`https://chatgpt.com/backend-api/codex/responses`. No endpoint or issuer override is accepted in
production.

The ChatGPT Codex backend is an unsupported internal service, not a stable public API. Account
entitlement, accepted models, response fields, and availability can change without notice. Kit
fails closed on unknown or malformed protocol transitions rather than emulating another client.
The configured subscription model aliases are therefore recorded as one mutable-model blocker;
the `OpenAI-Model` header, or the validated observed response model when absent, is reported as the
actual response identity.

Stateless tool follow-ups retain bounded continuation metadata under `openai.subscription.v1` on
the existing redacted reasoning and tool-call transcript parts. Kit persists that transcript as-is
and replays exact ordered encrypted reasoning and function calls only for the same account login
generation, model, and session. Incomplete, failed, and mismatched continuations are never replayed.

Before dispatch, Kit performs exactly one bounded internal retry for a 429 or known overload that
the backend reports before inference dispatch, using the same idempotency key. A second such
response is terminal; the durable adapter does not perform an outer retry.
