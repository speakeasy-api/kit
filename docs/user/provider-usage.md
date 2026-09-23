# Provider usage

Use `kit usage [provider]` to inspect provider usage and quota without starting a
model prompt:

```sh
kit usage
kit usage openai
kit usage openrouter --credential-store keychain
```

Supported providers are `openai` (also `openai-subscription`) and `openrouter`.
Omit the provider to check authenticated supported providers. The command uses the effective
credential storage from configuration and CLI overrides (`--credential-store` and
`--credential-dir`). OpenRouter also uses the effective `--openrouter-api-key` or
`OPENROUTER_API_KEY` override. Missing credentials for an explicitly selected provider or unavailable provider data
are reported rather than treated as zero usage. Provider usage is account/key-level
information, not the current session's accumulated token count.

In the TUI, `/usage [provider]` performs the same check in the background and shows
local informational output. It does not send a model prompt.

OpenAI reports **ChatGPT subscription/Codex** quotas, not every ChatGPT product
quota or API billing balance. Every reported main and additional rate-limit window
shows its interval, percentage remaining, and provider-reported reset
countdown and absolute UTC time. Fixed-duration windows are not calendar months. Missing quota information does not mean unlimited.

OpenRouter reports API-key spend in USD, including BYOK spend. The counters cover
all time and the current UTC day, Monday–Sunday week, and calendar month. The key's
remaining budget is reported directly; lifetime spend is not subtracted from a
recurring cap. No key cap does not imply unlimited account credits. Budget recurrence
is shown separately: a nonrecurring cap has no recurring reset, and an exact next
budget reset timestamp is not reported by this endpoint.

Checks run concurrently with bounded HTTP timeouts and response sizes. Successful
providers remain visible if another provider fails. Usage calls use the providers'
canonical usage endpoints, not custom model API base URLs. OpenAI authentication
may refresh through the existing login mechanism; no model requests are made.

## Related documentation

- [Configuration and authentication](getting-started-and-configuration.md)
- [TUI and sessions](tui-and-sessions.md)
