# Cerebras

Kit supports Cerebras as a distinct provider. Cerebras keys are not OpenRouter keys; they are sent to Cerebras's API, not to OpenRouter.

## Configure a key

Create a key in the [Cerebras Cloud Console](https://cloud.cerebras.ai). Supply it through `CEREBRAS_API_KEY` or `--cerebras-api-key`. Prefer an environment variable populated by your secret manager: command-line arguments can appear in shell history and process listings. Select the provider with `--provider cerebras` and a Cerebras model ID, such as `--model gpt-oss-120b`.

To save an explicitly supplied key, run `kit auth login cerebras` with the environment variable set (or with the key flag). Kit uses its configured credential storage backend. Login stores the key; it does **not** verify the key with Cerebras. `kit auth status cerebras` reports the configured source, not remote account validity, and never prints the key. Explicit flag/environment keys take precedence over stored credentials.

`kit auth logout cerebras` removes the locally stored key only. It does not clear a shell environment variable or revoke a key at Cerebras. Delete the key in the Cloud Console to revoke it. Cerebras documents keys as non-expiring static secrets; rotate them periodically. See [API key management](https://inference-docs.cerebras.ai/console/api-keys).

## Endpoint and model selection

Cerebras documents an OpenAI-compatible base URL of `https://api.cerebras.ai/v1`. Chat requests use `POST /v1/chat/completions` with `Authorization: Bearer <key>`. Compatibility is not a promise that every OpenAI or OpenRouter option is supported. See [OpenAI compatibility](https://inference-docs.cerebras.ai/resources/openai) and the [chat-completions reference](https://inference-docs.cerebras.ai/api-reference/chat-completions).

The public [model catalog](https://inference-docs.cerebras.ai/models/overview) checked for this implementation lists:

| API model ID | Documented context, free / paid |
| --- | --- |
| `gpt-oss-120b` | 65k / 131k tokens |
| `qwen-3.8-27b` | 64k / 128k tokens |

Kit's selector includes these public models and retains an explicitly selected custom model. For context gauges and compaction, Kit uses conservative free-tier baselines of 65,000 tokens for `gpt-oss-120b` and 64,000 for `qwen-3.8-27b`; it does not infer your account tier or assume a limit for unknown models. Paid accounts may therefore compact before their full allowance.

These are the catalog's rounded context values, not guaranteed account allowances. Availability, output limits, and rate limits vary by model and account tier. Context includes input and generated tokens; reasoning tokens also count toward the completion budget. Do not assume a model's maximum advertised context is available on the free tier. Trial-only and dedicated-endpoint models are not interchangeable with public shared models. Recheck the catalog before relying on a model ID or limit.

## Kit behavior

- Select models with `--model` or `/model cerebras:qwen-3.8-27b`. Provider selection does not automatically replace a configured model.
- `--reasoning-effort low|medium|high|default` uses Cerebras's flat `reasoning_effort` field. `default` leaves the choice to Cerebras; Kit does not currently expose Qwen's `none` effort.
- The dedicated provider always uses Cerebras's HTTPS endpoint. `OPENROUTER_BASE_URL`, `OPENROUTER_API_KEY`, and other OpenRouter request overrides do not configure Cerebras.
- Kit streams replies and assembles function arguments before executing tools. It omits OpenRouter-specific reasoning, cache, attribution, and stream-usage options.
- Initial system/context messages are combined into one system message. Later context messages retain their position but use the user role because Cerebras only permits a system message at the start. This changes the wire representation, not the stored transcript.
- Built-in Kit subagents inherit the separately held key through their process environment, never through argv or transcript content. External ACP harnesses do not receive either provider's API key from Kit.
- Upstream failure bodies are not displayed, to avoid echoing credentials or prompt content. Status codes identify HTTP failures; rate limits still require reducing request load or checking your account limits.

For persistent authentication, choose the same backend for login and runtime, for example `--credential-store keychain`, or `--credential-store file --credential-dir /private/path`. Standalone login cannot use the default memory backend.

## API capabilities and compatibility notes

The following summarizes upstream API behavior, not a guarantee that every option has a corresponding Kit CLI flag:

- **Output budget:** `max_completion_tokens` includes reasoning tokens. `max_tokens` is an alias; do not send both.
- **Sampling:** The API reference defines `temperature` (0–2), `top_p` (0–1), presence/frequency penalties (-2–2), `seed` (best-effort determinism), and up to four stop sequences. Actual support can differ by model. Prefer changing temperature or top-p, not both.
- **Reasoning:** `gpt-oss-120b` supports `low`, `medium`, and `high`, defaulting to `medium`. Qwen 3.8 supports `none`, `low`, `medium`, and `high`, defaulting to `high`. Reasoning effort is not an exact token budget. Consult the [reasoning guide](https://inference-docs.cerebras.ai/capabilities/reasoning) rather than assuming every model supports identical modes.
- **Tools:** Public GPT OSS and Qwen models support function tools, multi-turn calls, parallel calls, and `tool_choice` values `none`, `auto`, `required`, or a named function. The application executes tools and returns results; the model does not execute them. `parallel_tool_calls` defaults to true in the API reference. Strict schemas have model-specific requirements. See [tool calling](https://inference-docs.cerebras.ai/capabilities/tool-use).
- **Structured output:** Do not combine `tools` and `response_format` unless the selected model explicitly supports the combination. GPT OSS rejects requests with both fields. Use separate tool and formatting requests when necessary. See [OpenAI compatibility](https://inference-docs.cerebras.ai/resources/openai).
- **Streaming:** Set `stream: true` for incremental chat-completion chunks. User-facing text arrives in `choices[].delta.content`; reasoning may arrive separately in `delta.reasoning`. Tool calls require assembling streamed function arguments before execution rather than treating each chunk as complete JSON. See [streaming responses](https://inference-docs.cerebras.ai/capabilities/streaming) and the [chat-completions schema](https://inference-docs.cerebras.ai/api-reference/chat-completions).

## Credential implementation

The provider uses a separate redacted, zeroized `CerebrasApiKey` type. Stored credentials reuse Kit's existing memory, OS keychain, or private-file backend under the new `cerebras` / `default` namespace. The record is strictly `{ "api_key": "…" }`; no existing provider record or configuration schema is changed. File storage is permission-protected, not encrypted. Diagnostics do not include key values or raw malformed credential contents.

The library lookup `provider::cerebras_api_key(&storage)` reads only stored credentials; it does not inspect environment variables. CLI code resolves explicit sources separately. `provider::execute_cerebras_auth(command, &storage, active_key)` accepts `CerebrasAuthCommand::{Login, Status, Logout { local_only }}` and an optional `(&CerebrasApiKey, CerebrasApiKeySource)`; sources are `Flag` and `Environment`. Both logout modes are local-only because this integration does not implement remote revocation.
