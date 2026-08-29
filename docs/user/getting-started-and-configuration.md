# Getting Started and Configuring Kit

Kit is a coding agent runtime and terminal client. Choose a project working directory, authenticate the model provider, and then use the installed `kit` binary interactively, for one prompt, or as an ACP/A2A server. Run `kit --help` and `kit <command> --help` for the current, exhaustive command-line reference.

Run `kit init` to write the recommended `~/.kit/config.toml` and an empty `~/.kit/mcp.json` when those files do not exist. It selects `gpt-5.6-sol` and file-backed credentials in `~/.kit/credentials`. The command leaves existing files unchanged.

## Install and verify the `kit` binary

Install the latest packaged release with mise, then verify that the executable is on `PATH`:

```sh
mise use -g github:speakeasy-api/kit
kit --version
kit --help
```

A version can be pinned with a mise package such as `github:speakeasy-api/kit@0.1.83`. The examples below invoke the installed binary directly; they do not use `cargo run`.

## Choose a provider and authenticate

Kit supports `openai-subscription`, `openrouter`, and `speakeasy`. The default provider is `openai-subscription`, and the default model is `gpt-5.4`. A `--provider` or `--model` command-line value overrides the corresponding value in `~/.kit/config.toml`.

### ChatGPT subscription with native OpenAI login

Authenticate directly with Kit, then start the runtime:

```sh
kit auth login openai --credential-store keychain
kit auth status openai --credential-store keychain
kit tui --root /path/to/project --credential-store keychain
```

Login opens OpenAI's browser authorization flow and accepts the callback only on
`http://localhost:1455/auth/callback` or port 1457. Kit uses PKCE plus state and
nonce checks and validates RS256 access and ID tokens against OpenAI's JWKS.
OpenAI and MCP use one selected credential backend. The default is process-local
`memory`, but standalone OpenAI login rejects it; select persistent `keychain` or
`file` storage, including `--credential-dir` when selecting `file`.

Kit refreshes credentials within five minutes of expiry. Refreshes are synchronized
across threads and processes, preserve the authenticated account and credential
generation, and are forced once after a 401 response. OpenAI subscription turns can
retry up to 25 times within a 10-minute budget with deterministic full-jitter
exponential backoff capped at 30 seconds. Retries cover selected transient HTTP
statuses, request transport failures, explicit transient provider events, and stream
failures before the first model event. Kit reuses the request body, idempotency key,
and available turn state. Authentication, invalid requests, quota/billing failures,
unsupported responses, and failures after observable model output remain terminal.
Use `kit auth status openai` to check the credential.
`kit auth logout openai` revokes the refresh token before local deletion and keeps
the local credential if revocation fails. `kit auth logout openai --local-only`
skips revocation and prints a warning.

### OpenRouter API key and model

Set `OPENROUTER_API_KEY`, select `openrouter`, and use an OpenRouter model identifier:

```sh
export OPENROUTER_API_KEY=sk-or-v1-...
kit prompt --provider openrouter \
  --model anthropic/claude-sonnet-4 \
  --root /path/to/project \
  "Reply with a short project summary"
```

Kit uses the CLI or TOML `model`; `OPENROUTER_MODEL` does not override that selection. The adapter also accepts `OPENROUTER_BASE_URL`, `OPENROUTER_APP_NAME`, `OPENROUTER_SITE_URL`, `OPENROUTER_MAX_COMPLETION_TOKENS`, `OPENROUTER_TEMPERATURE`, and `OPENROUTER_REASONING_EFFORT`. Its model-catalog lookup for the selected model's context length is best-effort, so a catalog failure does not by itself prevent normal provider usage.

### Speakeasy AI Control Plane

Sign in through the Speakeasy dashboard, then use the same persistent credential
store for runtime commands:

```sh
kit auth login speakeasy --credential-store keychain
kit auth status speakeasy --credential-store keychain
kit prompt --credential-store keychain \
  --provider speakeasy --model anthropic/claude-sonnet-4 \
  --root /path/to/project "Reply with a short project summary"
```

Kit requests a producer-scoped Gram API key through a loopback form POST, verifies
the key and its accessible projects with `app.getgram.ai`, and stores the key and
selected project. Runtime requests use `Gram-Key` and `Gram-Project` against
`https://app.getgram.ai/chat/completions`. Kit deterministically maps its durable
session ID to `Gram-Chat-ID`, keeping new and resumed conversations together in AI
Control Plane Agent Sessions. Speakeasy does not currently provide a
public model catalog, so Kit uses OpenRouter's catalog as a v0 selector fallback; a
model outside Speakeasy's allowlist can fail at runtime. Logout removes the local
credential. Revoke the API key remotely in the Speakeasy dashboard.

## Run common command workflows

The working directory must exist and be a directory. If `--root` is omitted, Kit uses configured `root`, then `.`. A bad path reports `could not open working directory`; a non-directory reports `working directory is not a directory`. This directory supplies project context and relative-path resolution; it does not restrict filesystem access.

### Interactive terminal client with `kit tui`

Start the ACP-backed terminal client at a project root:

```sh
kit tui --root /path/to/project
```

Resume a persisted conversation by its displayed session ID:

```sh
kit tui --root /path/to/project --resume <session-id>
```

If a dead process left a stale session lock, `--force` can accompany `--resume`. It is not a general overwrite option and Clap rejects it without a resume argument.

### One-shot automation with `kit prompt`

Run one prompt and exit:

```sh
kit prompt --root /path/to/project "Summarize the project in five bullets"
```

`kit prompt` prints the response and then `session_id: <id>`. Continue that persisted session with:

```sh
kit prompt --root /path/to/project \
  --resume <session-id> \
  "Now identify the highest-risk module"
```

### ACP and A2A server commands

Use `kit serve` for ACP v1 on stdio plus selectable HTTP protocol surfaces:

```sh
kit serve --root /path/to/project                         # A2A
kit serve --root /path/to/project --remote-acp            # A2A and remote ACP
kit serve --root /path/to/project --remote-acp --no-a2a   # remote ACP only
kit serve --root /path/to/project --http 127.0.0.1:7331
kit serve --remote-acp --no-a2a --no-stdio --http 0.0.0.0:8081 # daemon
```

Without `--a2a` (or its `--http` alias), `serve` binds an available loopback port. Remote ACP v1 and v2 negotiate on the standard `/acp` endpoint; `/acp/v2` is an explicit v2-only alias. Both routes use the same HTTP listener, bearer-token policy, and HTTP/SSE or WebSocket transports. The `kit serve` stdio connection remains ACP v1. Stdout remains reserved for ACP, so local stdio and remote ACP can run together. Add `--no-stdio` for a foreground daemon that does not depend on stdin; this option requires `--remote-acp`. SIGINT and, on Unix, SIGTERM stop accepts, interrupt active ACP sessions, and allow about five seconds for concurrent cleanup before remaining session actors are aborted.

Add `--server-credential-file /private/token` to require the file's single bearer token for every request on the HTTP listener. A non-loopback daemon must not be exposed without authentication and suitable network controls. Use `kit acp` when the host needs only ACP on stdio and no HTTP listener. Select the wire version explicitly with `--protocol-version 1|2`; omitting it defaults to ACP v1:

```sh
kit acp --root /path/to/project --protocol-version 1
kit acp --root /path/to/project --protocol-version 2
```

## Configure `~/.kit/config.toml`

At startup, every runtime and authentication command attempts to load `$HOME/.kit/config.toml`. An absent file is allowed. If `HOME` is unset or empty, Kit uses built-in defaults without loading a home config. Unknown keys are ignored so configurations remain compatible across Kit versions. Invalid values, an unreadable file, and invalid TOML syntax are errors.

A representative configuration is:

```toml
root = "/path/to/project"
provider = "openai-subscription" # or "openrouter" or "speakeasy"
model = "gpt-5.4"
reasoning_effort = "medium" # low, medium, or high
a2a = "127.0.0.1:7331"
otel_endpoint = "http://localhost:4317"
otel_capture_message_content = false
otel_message_content_max_messages = 64
otel_message_content_max_bytes = 16384

# Optional: explicit MCP servers overlay any same-named plugin servers.
mcp_config = "/path/to/mcp.json"
credential_store = "file" # "memory", "keychain", or "file"
credential_dir = "/path/to/private/credentials"

[acp.review]
command = "review-agent"
args = ["acp"]
permissions = "deny" # "deny" or "cancel"

[subagent]
harness = "acp.review"

[subagent.harnesses."acp.review"]
allow_model_overrides = ["provider:model-a"]

[subagent.harnesses."acp.review".models]
review = "provider:model-a"

[plugins.local-plugin]
source = "path"
path = "./plugins/local-plugin"

[plugins.remote-plugin]
source = "archive"
url = "https://example.com/plugin.tar.gz"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

`root`, `provider`, `model`, and credential settings apply to all four runtime commands.
The optional `[resilience]` table applies one AgentKit retry and timeout policy to whichever provider is selected, including providers selected later with `/model` and built-in `acp.kit` children. Duration names include their units. The two timeout fields can be omitted to disable those timeouts; the other fields are required:

```toml
[resilience]
max_retries = 5
retry_budget_ms = 60000
attempt_timeout_ms = 30000       # omit to disable the per-attempt timeout
stream_idle_timeout_ms = 30000   # omit to disable the stream-idle timeout
initial_backoff_ms = 200
max_backoff_ms = 10000
```

Without this table, OpenRouter and Speakeasy remain single-attempt. OpenAI subscription retains Kit's built-in long-running policy: a 24-hour request budget, 10-minute attempt timeout, 5-minute stream-idle timeout, 60-second maximum exponential backoff, and 10-minute maximum server-directed retry delay. Invalid zero budgets/timeouts, unknown fields, and a maximum backoff smaller than the initial backoff fail configuration loading without rewriting the file.

 Subagent model aliases and explicit-override allowlists are scoped by fully qualified harness under `[subagent.harnesses."acp.name"]`. Omitting `allow_model_overrides` permits all explicit model selections accepted by that harness; an empty list disables explicit model overrides. This policy does not restrict the harness's inherited or default model.

`a2a` applies to `serve` and `tui`. Configured plugins can provide MCP servers without `mcp_config`; supported `stdio` and `streamable-http` declarations are registered, while `sse` declarations are skipped with a stderr diagnostic. If `mcp_config` is also set, its same-named entries override plugin servers, and live removal of an override restores the plugin server. Plugin data is stored under `<config-directory>/plugin-data/<plugin-manifest-name>`. See [Agent Plugins](agent-plugins.md) for placeholders, collision rules, and ACP child behavior. `otel_endpoint` enables OTLP/gRPC export of AgentKit's GenAI trace spans. Use a collector endpoint such as `http://localhost:4317` without a `/v1/traces` suffix. `credential_store` selects one backend for OpenAI, Speakeasy, and MCP and defaults to `memory`; selecting `file` requires `credential_dir`, while a credential directory is invalid with `memory` or `keychain`. Memory credentials are process-local and are not shared with the TUI server process or nested Kit children. Standalone OpenAI and Speakeasy login requires persistent `keychain` or `file` storage. ACP profiles are direct executable-and-argument configurations, not shell command strings. `[subagent].harness` must name an available fully qualified profile such as `acp.review`; otherwise startup reports `unknown subagent ACP harness`. When no subagent harness is selected, the built-in `acp.kit` profile is used.

### Configuration precedence and built-in defaults

For settings exposed by a command, precedence is:

1. command-line options, such as `--root`, `--provider`, or `--model`;
2. values in `~/.kit/config.toml`;
3. built-in defaults.

The OpenTelemetry endpoint follows the same CLI-over-TOML precedence, then falls
back to the standard `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable. If none
is set, trace export is disabled. Message capture is disabled by default because
structured prompts, tool arguments, outputs, file content, and compaction summaries
can contain secrets. Kit resolves `--otel-capture-message-content BOOL`, the TOML
`otel_capture_message_content` value, and
`OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` in that order. The environment
value must be `true` or `false` (case-insensitive). CLI or TOML can bound capture
with `otel_message_content_max_messages` (1–1024, default 64) and
`otel_message_content_max_bytes` (1–1048576, default 16384) per input or output
attribute. Enabling capture configures both the `gen_ai.input.messages` and
`gen_ai.output.messages` structured message arrays. Input capture keeps the newest
bounded tail, while output capture keeps the bounded head. If source content exceeds
the byte budget, Kit emits a structured truncation entry instead of partial content;
the arrays remain valid JSON and truncation does not split UTF-8. Capture applies to
main agents, ACP sessions, nested in-process agents, and compaction summarizer prompts
and outputs. Kit forwards all resolved values, including an explicit false, to its
TUI server and nested `acp.kit` children. Kit's OTLP subscriber exports only AgentKit
loop and MCP semantic targets; dependency spans and source location, thread, tracing
target, and span busy/idle metadata are omitted. For example:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 kit prompt "Summarize this project"
kit prompt --otel-endpoint http://localhost:4317 "Summarize this project"
kit prompt --otel-capture-message-content true --otel-message-content-max-messages 32 "Summarize this project"
```

The main built-in defaults are:

| Setting | Default |
| --- | --- |
| `root` | `.` |
| `provider` | `openai-subscription` |
| `model` | `gpt-5.4` |
| `credential_store` | `memory` |
| `serve`/`tui` A2A address | an available loopback port |
| subagent harness | `acp.kit` |

For example, with `model = "gpt-5.4"` in TOML, `kit prompt --model anthropic/claude-sonnet-4 ...` uses the CLI model. Omitting both CLI and TOML model selections uses `gpt-5.4`. The same CLI-over-TOML rule applies to `reasoning_effort` on `serve`, `acp`, `prompt`, and `tui`. Use `--reasoning-effort default` to override a configured value and leave effort unset. This preserves provider defaults, including `OPENROUTER_REASONING_EFFORT`. Environment variables used by a provider supply credentials or provider adapter settings; they do not change CLI-over-TOML precedence.

## Project instructions from `AGENTS.md`

Kit discovers `AGENTS.md` files at the project working directory and its ancestor directories and loads their content into the initial transcript as agent context. Put repository-wide guidance in a higher-level `AGENTS.md` and project-specific guidance nearer the selected root. Choose `--root` deliberately because it controls both the working directory and which `AGENTS.md` instruction chain is loaded.

If startup reports `could not load AGENTS.md context`, inspect the `AGENTS.md` files at the root and above it for read or loading problems. Changing `~/.kit/config.toml` does not replace project instructions; configuration selects runtime behavior, while `AGENTS.md` supplies instructions to the agent.

## Agent Skills

Kit discovers [Agent Skills](https://agentskills.io/) recursively from `<root>/.agents/skills` and `~/.agents/skills`. Validated [Agent Plugins](agent-plugins.md) can add exact skill directories and supported MCP servers from local packages or checksum-pinned archives. Collision precedence for skills is project skills, user skills, then plugins in lexical alias order. Project skills therefore override user and plugin skills with the same name. Each skill lives in a directory containing `SKILL.md`; its frontmatter must include a `name` using lowercase letters, digits, and hyphens that matches the directory name and a non-empty `description`.

```markdown
---
name: review
description: Review code changes for correctness.
---

Review the change and run the smallest relevant checks.
```

The `skill` entry in `compose` initially discloses only valid skill names and descriptions. When a task matches, the agent loads the skill before proceeding; the result contains the full Markdown body, skill directory, and paths to supporting resources. Project and user skill files are read with the Kit process's normal host permissions. Invalid or unreadable skills are omitted, and the same skill can be loaded repeatedly.

The hidden-tool catalog is captured when Kit creates the session's compose source. Restart the session after adding or removing a skill so its advertised schema is refreshed.

## Troubleshoot startup and configuration

- **`invalid config ...`**: validate known values and TOML types. Unknown fields are ignored; provider values are exactly `openai-subscription`, `openrouter`, and `speakeasy`, and credential-store values are `memory`, `keychain`, and `file`.
- **`could not read config ...`**: check permissions and that `$HOME/.kit/config.toml` is a readable regular file. Deleting an unwanted config is valid because a missing file falls back to defaults.
- **`credential_dir is required when credential_store is file`**: add the directory or choose another store.
- **`credential_dir requires credential_store to be file`**: remove the directory or select `file`.
- **Unexpected project or model**: check the command line first, then `~/.kit/config.toml`, then the built-in defaults. Use `kit <command> --help` to confirm which options that command accepts.
- **Provider authentication failure**: for `openai-subscription`, use the same persistent `--credential-store keychain` or `--credential-store file --credential-dir ...` for login, status, and runtime commands; standalone login rejects `memory`. Also check that callback ports 1455 or 1457 are free. For `openrouter`, check `OPENROUTER_API_KEY` and the selected OpenRouter model identifier. For `speakeasy`, use the same persistent credential store for login and runtime commands, and confirm that the stored project can access the selected model.
