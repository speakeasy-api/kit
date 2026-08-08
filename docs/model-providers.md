# Model providers

Kit supports named persistent provider profiles and the original environment-only setup. When
`KIT_PROVIDER` is set, Kit preserves the environment-only behavior described below and ignores the
persistent selection. Otherwise, the daemon loads the current persistent profile. Changing the
current profile requires a daemon restart.

The default registry path is `$XDG_CONFIG_HOME/kit/config.json`, or
`$HOME/.config/kit/config.json` when `XDG_CONFIG_HOME` is unset. Windows uses
`%APPDATA%\Kit\config.json`. `KIT_CONFIG_FILE` explicitly overrides the path. Kit never searches
the current working directory for provider configuration.

The registry contains plaintext credentials. Kit creates its directory with mode `0700` and the
file with mode `0600` on Unix. Do not share, commit, or relax permissions on this file; Kit rejects
symlinks, non-regular files, files larger than 64 KiB, and files with group or other permission
bits.

Create and select profiles without putting credentials on the command line:

```sh
export OPENROUTER_API_KEY=...
kit provider add openrouter --provider openrouter

export ANTHROPIC_API_KEY=...
kit provider add work --provider anthropic \
  --model claude-sonnet-4-5 --max-tokens 4096

kit provider add local --provider ollama --model llama3.1:8b
kit provider list
kit provider use local
# Restart the daemon after `provider use`.
```

OpenAI reads `OPENAI_API_KEY` by default. OpenRouter reads `OPENROUTER_API_KEY`. Anthropic prefers
`ANTHROPIC_AUTH_TOKEN` when present and otherwise reads `ANTHROPIC_API_KEY`. Use `--api-key-env ENV`
or `--auth-token-env ENV` only for nonstandard variable names; credentials are never accepted as
raw command-line values.

ChatGPT subscription authentication is separate from OpenAI API-key authentication. The native
browser login stores tokens only in the operating system keyring; the provider profile stores only
the selected model:

```sh
kit auth login openai
kit auth status openai
kit provider add chatgpt --provider openai-subscription --model gpt-5.6-sol
kit provider use chatgpt
# Restart the daemon after `provider use`.

kit auth logout openai
# If remote revocation is unavailable, explicitly delete only the local keyring record:
kit auth logout openai --local-only
```

`kit auth status openai` reports the keyring-backed account, email, and plan when available and
never prints tokens. Logging out removes the shared subscription credential; it does not remove the
credential-free profile. API-key `openai` profiles and the `OPENAI_API_KEY` environment override
continue to use the setup documented above and below.

`kit provider path`, `list`, `add`, and `use` are local commands. They do not discover or start a
daemon. `add` refuses an existing name unless `--replace` is supplied; the first profile becomes
current automatically, while later additions preserve the current profile. Use `--json` or
`--format json` for structured output. Listings never include credentials.

The strict JSON shape is:

```json
{
  "current": "work",
  "providers": {
    "work": {
      "provider": "anthropic",
      "api_key": "...",
      "model": "claude-sonnet-4-5",
      "max_tokens": 4096
    },
    "local": { "provider": "ollama", "model": "llama3.1:8b" }
  }
}
```

Profile names are 1-64 ASCII letters, digits, `.`, `_`, or `-`. Unknown fields and duplicate JSON
keys are rejected. Editing malformed or insecure configuration causes an actionable daemon setup
failure rather than an environment fallback.

## Environment-only override

Set `KIT_PROVIDER` to `openai`, `anthropic`, `openrouter`, or `ollama` to choose the built-in
provider. Kit then loads every AgentKit model adapter whose required environment variables are
valid. A persisted API run override selects the matching loaded adapter; requesting one that was
not configured fails that run with `provider_unavailable`. A missing, invalid, or unconfigured
`KIT_PROVIDER` fails daemon startup.

Provider settings are read by the corresponding AgentKit configuration:

| Provider | Required variables | Optional variables |
| --- | --- | --- |
| OpenAI | `OPENAI_API_KEY` | `OPENAI_MODEL` (default `gpt-4o`), `OPENAI_BASE_URL` (default `https://api.openai.com/v1/chat/completions`) |
| Anthropic | `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`; `ANTHROPIC_MODEL`; `ANTHROPIC_MAX_TOKENS` | `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com/v1/messages`), `ANTHROPIC_VERSION`, `ANTHROPIC_BETA` |
| OpenRouter | `OPENROUTER_API_KEY` | `OPENROUTER_MODEL` (default `openrouter/auto`), `OPENROUTER_BASE_URL` (default `https://openrouter.ai/api/v1/chat/completions`), `OPENROUTER_APP_NAME`, `OPENROUTER_SITE_URL`, `OPENROUTER_MAX_COMPLETION_TOKENS`, `OPENROUTER_TEMPERATURE`, `OPENROUTER_REASONING_EFFORT` |
| Ollama | `OLLAMA_MODEL` | `OLLAMA_BASE_URL` (default `http://localhost:11434/v1/chat/completions`) |

Every `_BASE_URL` above is a complete request endpoint, not an API root. Kit keeps a zeroized
credential copy in a `SecretLease` for redaction. The selected concrete AgentKit adapter also
necessarily holds its request credential for authentication. Effective run snapshots, events,
SQLite projections, and extension configuration contain provider/model identifiers but never
credential values.

Grammar-constrained edits are not selected from the environment. The versioned effective
run/experiment config and exact provider/model capability matrix control selection; see
`docs/operations/grammar-edit-output.md`. Kit supplies the normalized schema request while each
concrete AgentKit adapter remains responsible for provider-native wire projection.

Minimal CLI setups are:

```sh
KIT_PROVIDER=openai OPENAI_API_KEY=... kit --auto-start prompt ...
KIT_PROVIDER=anthropic ANTHROPIC_API_KEY=... ANTHROPIC_MODEL=claude-sonnet-4-5 ANTHROPIC_MAX_TOKENS=4096 kit --auto-start prompt ...
KIT_PROVIDER=openrouter OPENROUTER_API_KEY=... kit --auto-start prompt ...
KIT_PROVIDER=ollama OLLAMA_MODEL=llama3.1:8b kit --auto-start prompt ...
```

`--auto-start` starts the daemon with the provider environment above when no daemon is running.
To change provider configuration, stop the existing daemon and start a new one with the new
environment.

`deterministic-test` exists only in debug builds and is never a release fallback. It requires an
explicit `KIT_FAKE_PROVIDER` (`openai`, `anthropic`, `openrouter`, or `ollama`) and serves only runs
whose persisted effective provider matches that value. Debug deterministic runs retain
`KIT_FAKE_SCENARIO`, `KIT_FAKE_DELAY_MS`, `KIT_FAKE_BARRIER_ROOT`, and `KIT_FAKE_BARRIER_AT`.
