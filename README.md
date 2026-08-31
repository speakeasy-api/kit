<p align="center">
  <img src="docs/media/hero.gif" alt="Kit fixing a failing test suite from the terminal" width="900">
</p>

<h1 align="center">Kit</h1>

<p align="center">
  Kit is a coding agent runtime. It gives the model <strong>one tool</strong> for writing and running programs.<br>
  Kit provides a terminal client, an ACP server, an A2A endpoint, and a subagent orchestrator in one static binary.
</p>

<p align="center">
  <a href="https://github.com/speakeasy-api/kit/releases"><img alt="Release" src="https://img.shields.io/github/v/release/speakeasy-api/kit?display_name=tag&sort=semver"></a>
  <a href="https://github.com/speakeasy-api/kit/pkgs/container/kit"><img alt="Container" src="https://img.shields.io/badge/ghcr.io-speakeasy--api%2Fkit-blue"></a>
  <a href="https://github.com/speakeasy-api/kit/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/speakeasy-api/kit/actions/workflows/ci.yml/badge.svg"></a>
</p>

```sh
curl -fsSL https://raw.githubusercontent.com/speakeasy-api/kit/main/install.sh | sh
kit init && kit auth login openai && kit tui
```

---

## Why Kit

Most agent harnesses expose many tools and require one model round trip for each call. Kit exposes one tool: `compose`. The `compose` argument is a short program in [Runlet](https://github.com/danielkov/runlet). In one round trip, the program can read files, run tests, apply edits, retry failed commands, delegate work to subagents, and return structured data.

Fewer model round trips repeat less context and complete more work in each request. In size-matched production tasks, Kit used approximately half the input tokens and active time per hand-written line of Codex CLI or Claude Code ([comparison](#how-it-compares)).

- **One tool, unlimited composition.** The model receives one tool: `compose`. A `compose` program can call `shell`, `edit`, `subagent`, `prompt`, `fork`, `tool_search`, `tool`, `auth`, `skill`, `a2a`, and `docs`. Independent calls run concurrently. Use data dependencies or `after` blocks to control execution order. Use `boundary retry N` and `fail()` to handle errors in the program.
- **Reusable subagents.** A subagent is a reusable value. You can continue, fork, inspect, and close a subagent. You can require JSON that matches a schema. You can also use Claude Code, Codex, Cursor, or Kit as the subagent harness over ACP.
- **Open protocols.** Kit supports ACP v1 and v2 over stdio, HTTP/SSE, and WebSocket. Kit supports A2A v1 in both directions. It also supports MCP, Agent Skills, and Agent Plugin packages.
- **Long sessions.** Kit synchronizes each append-only JSONL transcript item to disk before it accepts the item. It compacts context automatically at 80% of the context window. You can resume a session from `tui`, `prompt`, or any ACP client. Kit also uses crash-safe locks. It retries eligible `openai-subscription` failures for up to 24 hours.
- **Model choice.** Kit supports ChatGPT subscriptions through native OAuth. It also supports models through OpenRouter and the Speakeasy AI Control Plane. Use `/model` to change the model during a session.
- **Small runtime.** Kit has no permissions framework, sandbox, or web UI. The `--root` option sets the working directory. Kit is a runtime, not a security boundary. Run Kit inside a security boundary that you trust. See the [trust model](docs/user/security-limits-and-troubleshooting.md).

## Install

### Install script (macOS arm64, Linux x86-64)

```sh
curl -fsSL https://raw.githubusercontent.com/speakeasy-api/kit/main/install.sh | sh
```

The installation script downloads the release archive. It verifies the archive against `SHA256SUMS` and installs Kit in `~/.local/bin`. Set `KIT_VERSION=v0.1.108` to select a release. Set `KIT_INSTALL_DIR` to change the destination.

<!-- PLACEHOLDER: record docs/media/install.gif — run the curl|sh line in a clean shell, then `kit --version`. The tape is in scripts/readme-media/install.tape; it only works once install.sh is on main. -->

### mise

```sh
mise use -g github:speakeasy-api/kit        # latest
mise use -g github:speakeasy-api/kit@0.1.108
```

### Docker

```sh
docker run --rm -it -v "$PWD:/workspace" -v ~/.kit:/home/kit/.kit \
  ghcr.io/speakeasy-api/kit:latest tui --credential-store file
```

Kit publishes images for `linux/amd64` and `linux/arm64`. Each image has three variants: `slim`, `bookworm`, and `alpine`. The default `slim` variant uses Debian slim. The `alpine` variant uses a native musl build.

Each release adds the tags `v<version>`, `v<version>-slim`, `v<version>-bookworm`, and `v<version>-alpine`. The newest stable release also updates the `latest`, `slim`, `bookworm`, and `alpine` tags.

Images run as the non-root `kit` user with UID 1000. The working directory is `/workspace`. Images do not include Git or language toolchains. Extend the image if you need these tools:

```dockerfile
FROM ghcr.io/speakeasy-api/kit:latest
USER root
RUN apt-get update && apt-get install -y --no-install-recommends git ripgrep && rm -rf /var/lib/apt/lists/*
USER kit
```

### From source

To build Kit from source, use the Rust toolchain specified in `rust-toolchain.toml`.

```sh
git clone https://github.com/speakeasy-api/kit && cd kit
cargo build --release          # target/release/kit
scripts/sign-release.sh        # macOS only: sign before using the Keychain credential store
```

Release binaries for macOS are signed and notarized under `com.speakeasy.kit`; the outer desktop app uses `com.speakeasy.kit.desktop`. See [Releasing](docs/releasing.md).

## Quick start

```sh
kit init                                   # ~/.kit/config.toml + ~/.kit/mcp.json
kit auth login openai                      # ChatGPT subscription, native OAuth (PKCE)
kit tui --root /path/to/project            # interactive
kit prompt --root /path/to/project "Summarize this repo in three lines"   # one-shot, prints session_id
kit acp --root /path/to/project            # ACP over stdio for editors and clients
kit serve --root /path/to/project --remote-acp --no-a2a --no-stdio --http 0.0.0.0:8081 \
  --server-credential-file /private/token  # headless daemon
```

OpenRouter and Speakeasy use the same login process. Run `kit auth login openrouter` or `kit auth login speakeasy`. Then add `--provider openrouter --model anthropic/claude-sonnet-5` to the Kit command that you run. Providers and MCP servers share one credential backend (`--credential-store keychain|file|memory`).

## Feature tour

### One tool: `compose`

The model writes a Runlet program. While the program runs, Kit shows the source in the transcript. Kit shows the status of each call and binding. Kit also shows loop and retry counters. When the program finishes, Kit replaces the source with the result.

<p align="center"><img src="docs/media/compose-live.gif" alt="A compose program running with live call annotations" width="900"></p>

```text
tests   = shell({ command: "python3 -m unittest -q" })
counts  = for path in text.split(text.trim(shell({ command: "ls *.py" }).stdout), "\n") {
  return { path, lines: number.parse(text.trim(shell({ command: "wc -l < " + path }).stdout)) }
}
fixed   = after tests { return edit({ path: "calc.py", hunks: [ ... ] }) }
return { tests: tests.success, counts, fixed }
```

`tests` and `counts` run concurrently. The `fixed` binding waits for `tests` because it uses `after`. A hunk edit uses `context_before`, `old`, and `context_after` as anchors. The anchors must match exactly once. Hunk edits do not require Git. Each file edit is atomic and preserves permissions and CRLF line endings. See [Compose and local tools](docs/user/compose-and-local-tools.md).

### Subagents you can steer, continue, and fork

A subagent is a Runlet value with an `id`, `output`, and `generation`. Use `prompt` to continue the session. Use `fork` to create a branch. Use `subagents({})` to list active subagents. Use `close` to stop a subagent. Kit rejects a stale generation. This check prevents two continuations from updating one session at the same time.

```text
review = subagent({
  harness: "acp.claude",
  prompt: "Review calc.py for edge cases.",
  output_schema: { type: "object", properties: { issues: { type: "array", items: { type: "string" } } }, required: ["issues"] }
})
ranked = prompt({ subagent: review, prompt: "Rank those issues by severity." })
alt    = fork({ subagent: ranked, prompt: "Now argue the opposite ranking." })
return { ranked: ranked.output, alt: alt.output, active: subagents({}) }
```

<p align="center"><img src="docs/media/subagents.gif" alt="Kit orchestrating Claude Code and Codex subagents in parallel" width="900"></p>

- **Any ACP harness.** Kit includes `acp.kit`. You can add Claude Code, Codex, Cursor, or another harness that supports ACP v1 over stdio. The TOML configuration below requires four lines. Kit reads each harness's `initialize` response at runtime. Kit uses native `session/fork` when the harness advertises it. Otherwise, Kit creates an isolated transcript fork for Kit children.
- **Structured output.** Set `output_schema` on any call that produces a turn. If a reply does not match the schema, Kit returns the raw text and advances the generation. The next `prompt` can repair the reply.
- **Per-harness model aliases.** `[subagent.harnesses."acp.claude".models] architect = "opus"` lets the model request `model: "architect"` without knowing the harness namespace. An `allow_model_overrides` list restricts the available models.
- **Bounded resources.** Subagent depth is limited to 2. Each session can have 120 live subagents. The startup handshake has a 30-second limit. Kit rejects or cancels permission requests from headless children. Kit never approves these requests automatically.
- **Explicit context.** Each child starts without the parent conversation history and receives only the prompt that you provide. An `acp.kit` child also inherits the working directory, `AGENTS.md` chain, provider, MCP configuration, and credentials. Skills load into a child on demand with `skill({ name })`.

See [Reusable subagents and ACP harnesses](docs/user/subagents-and-acp-harnesses.md).

### Background work without losing the turn

Set `background: true` to detach a `compose` program immediately. Set `background: 60` to keep the program in the foreground for one minute before detachment. The turn ends, but the conversation continues. Kit adds the result to the session when the program finishes. In the TUI, use `⌘B` to move the newest running call to the background. Use `^K` to stop the selected call. The model can stop a call with `close({ call_id })`.

<p align="center"><img src="docs/media/background.gif" alt="A background compose call completing after the turn already ended" width="900"></p>

### Steer a running turn

While the agent works, press `Enter` to add a message to the *current* turn through ACP v2 `steer`. Kit does not queue the message for the next turn. Press `Esc` or `^C` to interrupt a running turn. When Kit is idle, press `^C` to clear a nonempty prompt. Press `^C` again to exit after the prompt is empty.

<!-- PLACEHOLDER: record docs/media/steer.gif — start a longer task ("refactor calc.py into a class and add tests"), then while it is working type "actually keep it functional, just add docstrings" and press Enter. Capture the prompt placeholder changing from "message kit…" to "steer kit…" and the injected message appearing inside the running turn. -->

### Any model, one credential store

<p align="center"><img src="docs/media/model-picker.png" alt="The /model picker grouped by provider" width="900"></p>

| Provider | Login | Notes |
| --- | --- | --- |
| `openai-subscription` | `kit auth login openai` | Native ChatGPT OAuth uses PKCE, state, nonce, and RS256 tokens validated against OpenAI's JWKS. The callback uses loopback only. Kit refreshes tokens five minutes before expiry and synchronizes refreshes across processes. |
| `openrouter` | `kit auth login openrouter` or `OPENROUTER_API_KEY` | Kit loads a live model catalog. It uses context-window data for the gauge and compaction. |
| `speakeasy` | `kit auth login speakeasy` | Kit uses the Speakeasy AI Control Plane. Kit maps its session ID to the Gram chat ID so that a resumed conversation stays in one thread. |

`/model sonnet` switches the live session at the next safe turn boundary. Press `Tab` in the picker to also update `~/.kit/config.toml`. Use `/effort low|medium|high|default` to change the reasoning effort.

### Live MCP configuration

You do not need to restart Kit after you add a server. Kit merges plugins, configured `mcp_config`, project-root `.mcp.json`, and `--mcp-config` in that order, then reloads every named file before each `tool_search` and `auth` call. Kit waits for new servers to finish initialization before ranking tools.

Kit detects OAuth from the server's Bearer challenge. An `auth` block is not necessary. The model calls `auth({ name })` and gives you a URL. After you complete the browser flow, Kit resumes the session. If an access token expires, Kit refreshes the token and repeats the rejected call once.

<p align="center"><img src="docs/media/mcp.gif" alt="tool_search discovering an MCP web-search tool and calling it" width="900"></p>

```json
{ "mcpServers": {
    "exa":    { "url": "https://mcp.exa.ai/mcp", "description": "Hosted web search" },
    "linear": { "url": "https://mcp.linear.app/mcp", "description": "Issues and projects" },
    "local":  { "command": "my-mcp-server", "args": ["--stdio"] } } }
```

See [MCP](docs/user/mcp.md).

### Agent Plugins and Agent Skills

Kit loads validated [Agent Plugin](docs/user/agent-plugins.md) packages from a local directory or a SHA-256-pinned archive. Kit supports ZIP, tar.gz, tar, and GitHub tag archives.

Plugin skills become available in the `skill` catalog. Kit starts plugin `stdio` and `streamable-http` MCP servers without an `mcp.json` file.

Kit discovers [Agent Skills](https://agentskills.io/) in `<root>/.agents/skills` and `~/.agents/skills`. Kit initially shows only skill names and descriptions. Kit loads the full `SKILL.md` only when requested.

```toml
[plugins.review]
source = "path"
path = "./plugins/review"

[plugins.gram]
source = "archive"
url = "https://github.com/speakeasy-api/gram-plugin/archive/refs/tags/v1.2.0.tar.gz"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
subdir = "plugin"
```

<!-- PLACEHOLDER: record docs/media/plugins.gif — with a plugin configured in ~/.kit/config.toml, start `kit tui` and ask "Which skills and MCP servers do you have from plugins? Load the review skill." Capture the skill catalog + plugin MCP server in tool_search's `mcp` status listing. -->

### Persistent sessions and context compaction

Kit stores each conversation as an append-only JSONL transcript in `~/.kit/sessions`. Kit synchronizes each item to disk before it adds the item to memory.

You can resume a session from the TUI or with the `--resume` option of `kit prompt`. ACP clients can use `session/load` or `session/resume`.

When the provider reports 80% context-window use, Kit converts older history into a structured note. Kit persists the replacement. Kit retains the bootstrap instructions and a tool-safe tail. Use `/compact` to compact the history on demand.

<p align="center"><img src="docs/media/sessions.png" alt="The /sessions picker" width="900"></p>
<p align="center"><img src="docs/media/headless.gif" alt="kit prompt one-shot, then resuming the same session" width="900"></p>

<!-- PLACEHOLDER: record docs/media/long-session.png — resume a multi-hour session, send one small prompt, and screenshot the header once the gauge shows a high percentage (e.g. "78% 212k/272k"). Optionally a second frame right after the automatic checkpoint fires. -->

### Long-running and headless by design

- **Crash-safe.** Kit permits only one live process to modify each session. Kit reclaims a stale lock only when the operating system confirms that no process holds it. Use `--force` only with `--resume <session-id>` to reclaim a stale lock. The TUI also reclaims a stale lock when you switch sessions. Kit replaces an incomplete tool call with an explicit synthetic error result.
- **OpenAI subscription retries.** Kit retries eligible transient failures with deterministic full-jitter backoff. Eligible failures include 408, 425, 429, 500, 502, 503, 504, and 529 responses. They also include transport failures and stream failures before the first token. The retry deadline is 24 hours. Kit reuses the same idempotency key. `openai-subscription` does not retry authentication failures, quota failures, or failures after output starts.
- **Daemon operation.** `kit serve --remote-acp --no-stdio` runs independently of stdin. `SIGINT` and `SIGTERM` stop new connections, interrupt live sessions, and allow five seconds for draining. One bearer token file protects the HTTP listener.
- **Observability.** Set `otel_endpoint = "http://localhost:4317"` to export AgentKit GenAI spans through OTLP/gRPC. Kit exports spans for the main agent, ACP sessions, nested children, and the compactor. Kit disables message-content capture by default. Kit limits captured message content when you enable it.

<!-- PLACEHOLDER: screenshot docs/media/otel-trace.png — point otel_endpoint at a local Jaeger/Tempo, run a session with two subagents, screenshot the trace waterfall showing the nested gen_ai spans. -->

### Terminal client

`kit tui` starts `kit serve` and communicates with the server through ACP. The terminal client uses the same protocol as an editor. You can drag and drop image and audio attachments into the terminal. Terminals that support Kitty graphics, Sixel, or the iTerm2 inline image protocol show inline previews.

Click a code block to copy its contents. Use `^y` to copy the last response as Markdown through OSC 52. Use `^l` and `^t` to toggle the agent log and reasoning views. Use `^o` to collapse tool output. See [TUI and sessions](docs/user/tui-and-sessions.md) for the complete key table.

### Editors and other clients over ACP

Any ACP-compatible client can use Kit. Use `kit acp` for stdio. Use `kit serve --remote-acp` for HTTP/SSE or WebSocket connections at `/acp`. V2-only clients can use `/acp/v2`. Kit also supports A2A v1 on the same listener. Kit can call other A2A agents with `a2a({ url, prompt })`.

<!-- PLACEHOLDER: screenshot docs/media/acp-editor.png — Kit running as an external agent inside an ACP-capable editor (e.g. Zed: add {"agent_servers": {"Kit": {"command": "kit", "args": ["acp", "--root", "."]}}} to settings.json). Show a prompt with a compose tool card rendered by the editor. -->

## Configuration

`kit init` writes this configuration. Command-line flags override the configuration.

```toml
provider = "openai-subscription"   # openrouter | speakeasy
model = "gpt-5.6-sol"
reasoning_effort = "high"
credential_store = "file"          # memory | keychain | file
credential_dir = "~/.kit/credentials"
mcp_config = "~/.kit/mcp.json"
otel_endpoint = "http://localhost:4317"

[acp.claude]
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp@0.69.0"]
permissions = "deny"               # deny (default) | cancel — Kit never auto-approves

[acp.codex]
command = "npx"
args = ["-y", "@agentclientprotocol/codex-acp@1.4.0"]

[acp.cursor]
command = "cursor-agent"
args = ["acp"]

[subagent]
harness = "acp.kit"

[subagent.harnesses."acp.claude".models]
architect = "opus"

[subagent.harnesses."acp.kit".models]
flash = "openrouter:openai/gpt-5.6-luna"
```

Kit loads `AGENTS.md` files from the working directory and its ancestors as project instructions. See [Getting started and configuration](docs/user/getting-started-and-configuration.md) for all other settings.

## How it compares

Kit, Codex CLI, and Claude Code were used to complete production engineering tasks during July–August 2026. The observed work covered admin dashboards and billing flows, authentication and session fixes, telemetry, external integrations, and CI/build performance. All 16 resulting pull requests merged.

| Median per hand-written line (300–1000-line PRs) | Kit | Codex CLI | Claude Code |
| --- | ---: | ---: | ---: |
| Input tokens | **Best (49.7k)** | 127% more (113k) | 100% more (99.6k) |
| Output tokens | **Best (274)** | 17% more (321) | 19% more (325) |
| Active time | **Best (0.13 min)** | 138% more (0.31 min) | 138% more (0.31 min) |
| User messages per session | **Best (5)** | 180% more (14) | 120% more (11) |

Kit used the fewest tokens, took the least active time, and needed the least steering. Two Kit runs merged from a single message.

## Documentation

- [Getting started and configuration](docs/user/getting-started-and-configuration.md)
- [Compose and local tools](docs/user/compose-and-local-tools.md)
- [Reusable subagents and ACP harnesses](docs/user/subagents-and-acp-harnesses.md)
- [MCP](docs/user/mcp.md)
- [Agent Plugins](docs/user/agent-plugins.md)
- [TUI and sessions](docs/user/tui-and-sessions.md)
- [Security, limits, and troubleshooting](docs/user/security-limits-and-troubleshooting.md)
- [Releasing](docs/releasing.md) · [Third-party notices](THIRD_PARTY_NOTICES.md)

Kit compiles the same documentation into the binary. The agent searches this documentation with `docs({ query })`.

## Reporting issues

Report issues at [speakeasy-api/kit/issues](https://github.com/speakeasy-api/kit/issues). Include the Kit version and follow the [reporting guide](docs/user/reporting-kit-issues.md). An agent must ask for your permission before it files an issue for you.

## License

MIT

<p align="center">
  kit built by
  <br><br>
  <a href="https://speakeasy.com/">
    <img src="docs/media/speakeasy-icon.png" alt="Speakeasy" width="96">
  </a>
  <br>
  <a href="https://speakeasy.com/">speakeasy.com</a>
</p>
