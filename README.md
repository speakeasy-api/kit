# Kit

Kit is a coding agent runtime and terminal client.

It exposes:

- ACP over stdio for clients and editors
- A2A v1 JSON-RPC for agent collaboration
- a Ratatui ACP client
- one model-visible tool: `compose`, backed by released Runlet
- hidden compose children for bundled Kit documentation, shell commands, hunk edits, reusable ACP subagents, A2A calls, Agent Skills, and MCP discovery/authentication
- `AGENTS.md` instructions loaded from the working directory and its ancestors
- Agent Skills discovered from `<root>/.agents/skills`, `~/.agents/skills`, and validated local or checksum-pinned archive [Agent Plugins](docs/user/agent-plugins.md)
- OpenAI subscription access through Kit's native ChatGPT OAuth login
- OpenRouter through AgentKit's OpenRouter provider adapter
- Speakeasy AI Control Plane through its OpenRouter-compatible completions endpoint

It intentionally has no general interactive permissions framework, provenance ledger,
rollback system, control-plane authentication, or web UI.

## Install

Source code and prebuilt binaries are published from the public
[`speakeasy-api/kit`](https://github.com/speakeasy-api/kit) repository.
Install the latest release with mise:

```sh
mise use -g github:speakeasy-api/kit
kit --version
```

Pin a release by appending its version, for example
`github:speakeasy-api/kit@0.1.83`.

### Container image flavors

The repository Dockerfile builds three user-agnostic runtime flavors. The
unqualified `default` target aliases `slim`.

```sh
docker build --target bookworm -t kit:bookworm . # Debian/glibc
docker build --target slim -t kit:slim .         # Debian slim/glibc (default)
docker build --target alpine -t kit:alpine .     # Alpine/musl
docker build -t kit:local .                      # same as slim
```

Each release publishes public `linux/amd64` and `linux/arm64` images to
`ghcr.io/speakeasy-api/kit`. Versioned tags include `v<version>`,
`v<version>-slim`, `v<version>-bookworm`, and `v<version>-alpine`. The newest
stable release also updates `latest`, `slim`, `bookworm`, and `alpine`;
prereleases and older stable reruns do not update floating tags. GitHub creates a new GHCR package as private, so an
organization administrator must set the package visibility to public after its
first push; the release workflow verifies this setting. For example:

```sh
docker pull ghcr.io/speakeasy-api/kit:v0.1.87
```

The Debian flavors share a native glibc build; Alpine uses a separate native musl
build. All flavors include CA certificates and a POSIX shell, and run as the
non-root `kit` user with UID and GID `1000` from `/workspace`. Bind-mounted
workspaces must be writable by that identity.

The base images intentionally omit Git, language toolchains, and project-specific
utilities. A downstream image can install what it needs by switching to root and
then restoring the `kit` user:

```dockerfile
FROM ghcr.io/speakeasy-api/kit:v0.1.87-slim
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ripgrep \
    && rm -rf /var/lib/apt/lists/*
USER kit
```

Run `docker run --rm kit:local --version` to verify a local build.

## Run

Authenticate your ChatGPT subscription once, then start the TUI:

```sh
cargo run -- auth login openai --credential-store keychain
cargo run -- tui --root /path/to/project --credential-store keychain
```

The native login uses OAuth authorization code flow with PKCE, state, and nonce.
It listens only on registered loopback callback ports 1455 and 1457, validates
OpenAI RS256 tokens against the pinned JWKS endpoint, and uses the same selected
credential backend as MCP. Standalone login rejects the default `memory` backend;
select persistent `keychain` or `file` storage.
Protocol source attribution and reproduced upstream licenses are in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

Every TUI conversation is persisted as an append-only, versioned JSONL transcript
under `~/.kit/sessions`. Sessions from the legacy `<root>/.kit/sessions` location
remain resumable and are copied to the global location on resume. When reported
context use reaches 80% of the model's window, Kit automatically checkpoints older
history while preserving bootstrap instructions and a tool-safe recent tail; the
replacement is persisted for resume.
The session id is shown in the header. List workspace sessions or resume one with:

```sh
cargo run -- sessions --root /path/to/project
cargo run -- tui --root /path/to/project --resume <session-id>
```

`kit sessions` prints the shared newest-first session catalog with ID, updated time, title, and preview.

A per-session filesystem lock prevents two live Kit processes from mutating the
same transcript. If a crashed process left its lock file behind, add `--force`;
Kit will reclaim it only when no live process still holds the OS lock.

For a non-interactive smoke test, run one prompt and print the resumable session id:

```sh
cargo run -- prompt --root /path/to/project "Reply with a short project summary"
```

The command prints the model response followed by `session_id: ...` and exits.
Pass that id to `prompt --resume <session-id>` or `tui --resume <session-id>`.

Run ACP on stdio with an optional A2A and remote ACP HTTP listener, or use the dedicated stdio-only command:

```sh
cargo run -- serve --root /path/to/project                         # A2A HTTP
cargo run -- serve --root /path/to/project --remote-acp            # A2A + ACP HTTP
cargo run -- serve --root /path/to/project --remote-acp --no-a2a   # ACP HTTP only
cargo run -- serve --remote-acp --no-a2a --no-stdio --http 0.0.0.0:8081 # ACP daemon
cargo run -- acp --root /path/to/project                           # no HTTP listener
```

Both commands reserve stdout for ACP and send diagnostics to stderr. `serve`
chooses an available loopback port by default; pass `--a2a 127.0.0.1:7331`
(or its visible alias `--http`) to request a specific address. Remote ACP uses
HTTP/SSE or WebSocket at `/acp`, while A2A discovery uses the Agent Card
endpoint. Add `--server-credential-file /private/token` to require that file's
single bearer token on every HTTP request. `--no-stdio` requires `--remote-acp` and keeps
`serve` running independently of stdin. SIGINT and SIGTERM stop new connections, interrupt
active ACP sessions, and drain them for up to about five seconds before aborting stragglers.
`acp` never starts an HTTP listener. Never expose a non-loopback daemon without authentication
and appropriate network controls.

Inspect or remove the credential with `kit auth status openai` and
`kit auth logout openai`. Logout revokes the refresh token before deleting the
local record; if remote revocation is unavailable, credentials are retained. Use
`--local-only` only when you intentionally need deletion without revocation.
Kit refreshes expiring credentials proactively and retries one rejected credential
with a synchronized forced refresh.

To use OpenRouter, sign in to the shared credential store or supply an API key,
then select the provider and pass an OpenRouter model identifier:

```sh
cargo run -- auth login openrouter --credential-store keychain
cargo run -- auth status openrouter --credential-store keychain
cargo run -- prompt --credential-store keychain \
  --provider openrouter --model anthropic/claude-sonnet-4 \
  --root /path/to/project "Reply with a short project summary"
cargo run -- auth logout openrouter --credential-store keychain
```

Runtime and OpenRouter auth status resolve keys in this order: a non-empty global
`--openrouter-api-key`, a non-empty `OPENROUTER_API_KEY`, then the stored
`openrouter/default` credential. An empty flag is rejected; an empty environment value
is treated as absent. The flag is available to `serve`, `acp`, `prompt`, `tui`, and `auth`, but its value is
present in Kit's initial process argv and can be exposed by shell history or process
inspection. Prefer the environment or stored credentials. Kit never rewrites its own
process environment and never adds the key to child argv. It overrides
`OPENROUTER_API_KEY` only in the TUI's Kit `serve` child and built-in or configured
`acp.kit` children. Kit removes its resolved OpenRouter key from external ACP child
environments rather than exposing the credential to arbitrary configured agents.

`kit auth status openrouter` reports whether the active source is the flag,
environment, or stored credentials without printing the key. Logout removes only the
stored record and cannot revoke an OpenRouter key automatically. If the flag or
`OPENROUTER_API_KEY` remains set, logout warns that it is still active; the
environment value is retained.

Kit uses the selected `--model` or configured `model`; `OPENROUTER_MODEL` does not
override it. The adapter also honors its optional `OPENROUTER_BASE_URL`,
`OPENROUTER_APP_NAME`, `OPENROUTER_SITE_URL`, `OPENROUTER_MAX_COMPLETION_TOKENS`,
`OPENROUTER_TEMPERATURE`, and `OPENROUTER_REASONING_EFFORT` settings. Stored OAuth
credentials are restricted to OpenRouter's canonical API endpoint; custom base URLs
require an explicit flag or environment key.

Kit also reads the OpenRouter model catalog's
`context_length` for the selected model, enabling the normal context gauge and
80% automatic-compaction threshold. Catalog lookup is best-effort; an unknown
model or unavailable catalog leaves usage reporting intact but disables the gauge
and automatic compaction rather than guessing a window.

To sign in with Speakeasy, use the same persistent credential store for login and
runtime commands:

```sh
cargo run -- auth login speakeasy --credential-store keychain
cargo run -- prompt --credential-store keychain \
  --provider speakeasy --model anthropic/claude-sonnet-4 \
  --root /path/to/project "Reply with a short project summary"
```

Login opens `app.getgram.ai`, receives the producer-scoped Gram API key through a
loopback form POST, verifies its accessible projects, and stores the key and selected
project. Inference sends them as `Gram-Key` and `Gram-Project` to
`https://app.getgram.ai/chat/completions`. Kit also maps its durable session ID to
Gram's deterministic UUIDv5 `Gram-Chat-ID`, so new and resumed conversations appear
as the same session in AI Control Plane. OpenRouter settings
and credentials remain isolated. The AI Control Plane does not currently expose a
public model-catalog endpoint, so the Speakeasy selector uses the OpenRouter catalog
as a v0 fallback. Models outside Speakeasy's allowlist can be selected but fail at
runtime. `kit auth logout speakeasy` removes the local key; revoke it remotely in
the Speakeasy dashboard.

## Configuration

Run `kit init` to create `~/.kit/config.toml` and an empty `~/.kit/mcp.json` when they do not exist. The generated setup selects `gpt-5.6-sol` and file-backed credentials in `~/.kit/credentials`. Existing files are left unchanged.

Kit loads optional defaults from `~/.kit/config.toml`. Command-line values take
precedence, and omitted values that are not configured retain Kit's built-in
defaults (`root = "."`, `provider = "openai-subscription"`,
`model = "gpt-5.4"`, and in-memory OAuth credentials). Supported keys are:

```toml
root = "/path/to/project"
provider = "openai-subscription" # or openrouter or speakeasy
model = "gpt-5.4"
reasoning_effort = "medium" # low, medium, or high
a2a = "127.0.0.1:7331"
otel_endpoint = "http://localhost:4317"
otel_capture_message_content = false
otel_message_content_max_messages = 64
otel_message_content_max_bytes = 16384
mcp_config = "/path/to/mcp.json"
credential_store = "file" # memory, keychain, or file
credential_dir = "/path/to/private/credentials"

[acp.review]
command = "review-agent"
args = ["acp"]
permissions = "deny" # deny (default) or cancel

# Override only the Kit executable/base argv; Kit appends its runtime flags.
[acp.kit]
command = "kit"
args = ["acp"]

[subagent]
harness = "acp.review"
```

`root`, `provider`, `model`, and `reasoning_effort` apply to runtime commands. Credential settings also apply to authentication commands. `--reasoning-effort low|medium|high` overrides TOML; `--reasoning-effort default` removes Kit's explicit selection and preserves provider defaults (including `OPENROUTER_REASONING_EFFORT`). `a2a` supplies the shared HTTP listen address for
`serve` and `tui`. `otel_endpoint` enables OTLP/gRPC trace export for AgentKit's
GenAI spans. Use a collector endpoint such as `http://localhost:4317` without a
`/v1/traces` suffix. The endpoint can also be set with `--otel-endpoint` or
`OTEL_EXPORTER_OTLP_ENDPOINT`; CLI values take precedence over TOML, which takes
precedence over the environment. Message capture is off by default because prompts,
tool arguments, outputs, file content, and compaction summaries can contain secrets.
Kit resolves `--otel-capture-message-content BOOL`,
`otel_capture_message_content`, and
`OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` in that order and requires the
environment value to be `true` or `false`. The message and byte bounds can be set by
CLI or TOML and default to 64 messages and 16384 UTF-8 bytes per input or output
attribute. Enabling capture configures both the `gen_ai.input.messages` and
`gen_ai.output.messages` structured message arrays. Input capture keeps the newest
bounded tail; output capture keeps the bounded head. Content that exceeds the byte
budget is represented by a structured truncation entry, so each array remains valid
JSON and truncation never splits UTF-8. Capture applies to main agents, ACP sessions,
nested in-process agents, and compaction summarizer prompts and outputs. Kit forwards
the fully resolved settings, including an explicit false, to its TUI server and
nested `acp.kit` children. The OTLP subscriber exports only AgentKit loop/MCP
semantic targets and omits source location, thread, tracing target, and span
busy/idle metadata. ACP profiles are trusted, strict `command`/`args` argv
configurations: Kit does not invoke a shell and always sets the child cwd to Kit's
working directory. Multiple names may be configured. `[subagent].harness` selects the
default; references must use the fully qualified `acp.<name>` form. When
omitted, `acp.kit` runs the current executable with `acp` as its base argv. An
explicit `[acp.kit]` overrides that executable/base argv. In both cases Kit then
appends root, provider, model, the resolved reasoning effort, persistent session, resume, MCP, credential, and inherited
depth flags, and the profile remains eligible for isolated Kit transcript fork
fallback. Other profiles remain literal generic ACP argv. Configured child
processes inherit Kit's environment except for resolved OpenRouter credentials:
`acp.kit` receives the key as a child-only `OPENROUTER_API_KEY` override, while
generic ACP profiles have `OPENROUTER_API_KEY` removed.
Missing config files are ignored; unreadable or invalid files and unknown selected
harness references produce an error rather than being silently discarded.

### Generic ACP harnesses

Any ACP v1 agent that speaks newline-delimited JSON-RPC over stdio can be a
profile. Put only the executable in `command` and each argument in `args`; Kit
spawns it directly, so shell quoting, pipes, environment assignments, and
compound commands do not work. Use an executable on `PATH` for a portable
configuration, or an absolute path for a machine-specific one. The agent must
keep stdout protocol-only, may log to stderr, and must support `initialize`,
`session/new`, and `session/prompt`. Kit sets its cwd to Kit's working directory
and inherits the parent environment.

For example, these profiles pin the npm adapters while leaving the executable
lookup portable:

```toml
[acp.claude]
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp@0.63.0"]
permissions = "deny"

[acp.codex]
command = "npx"
args = ["-y", "@agentclientprotocol/codex-acp@1.4.0"]

[acp.cursor]
command = "cursor-agent"
args = ["acp"]

[subagent]
harness = "acp.claude"
```

Install Node.js for the npm-backed profiles and install the
[Cursor CLI](https://cursor.com/docs/cli/installation) for `cursor-agent`.
Authenticate each agent before starting Kit (`claude auth login`, `codex login`,
or `cursor-agent login` respectively); Kit does not initiate a generic agent's
ACP authentication flow. A generic child's permission request is handled by
the profile's fail-closed `permissions` policy: `deny` by default, or `cancel`.
Configure any additional non-interactive policy in the agent itself, with the
same care as for running that agent directly.

This compatibility snapshot records only the capabilities relevant to Kit,
based on each listed release's `initialize` response as inspected on 2026-08-16.
It is not a claim of full feature parity, and a different release may advertise
different capabilities.

| Agent | Version inspected | Profile argv | New session and prompt | Advertises `session/fork` | Kit `fork` |
| --- | --- | --- | --- | --- | --- |
| [Claude Agent ACP](https://github.com/agentclientprotocol/claude-agent-acp/tree/v0.63.0) | 0.63.0 (2026-07-27) | `npx -y @agentclientprotocol/claude-agent-acp@0.63.0` | Yes | Yes | Native ACP fork |
| [Codex ACP](https://github.com/agentclientprotocol/codex-acp/releases/tag/v1.4.0) | 1.4.0 (2026-08-16) | `npx -y @agentclientprotocol/codex-acp@1.4.0` | Yes | No | Unsupported for a generic profile |
| [Cursor CLI ACP](https://cursor.com/docs/cli/acp) | 2026.08.04-aaa8809 (checked 2026-08-16) | `cursor-agent acp` | Yes | No | Unsupported for a generic profile |

Kit decides from the initialization response at runtime rather than from this
table. If a generic agent does not advertise `session/fork`, ordinary `subagent`
and `prompt` calls still work, but `fork` returns an explicit unsupported error.
Only `acp.kit` has Kit's isolated transcript fallback.

Nested ACP permission policy is configured only in these trusted local profiles.
The default `permissions = "deny"` selects `reject_always`, or `reject_once` when
that is the only rejection offered; if the child offers no rejection, Kit
cancels the request. `permissions = "cancel"` always cancels. Kit never selects
an allow option: ACP permission requests contain no trustworthy, machine-verifiable
scope that could safely support unattended approval.

## Agent Plugins

Kit loads validated Agent Plugin packages from local directories and SHA-256-pinned ZIP, tar.gz, or tar URLs. Plugin skills join the existing `skill` catalog, and supported plugin MCP declarations work without an explicit MCP JSON file. Kit runs plugin `stdio` servers and connects `streamable-http` servers; deprecated `sse` entries are skipped with a stderr diagnostic. An explicit `--mcp-config` or `mcp_config` file overlays plugins by server name and remains live-reloadable. See [Agent Plugins](docs/user/agent-plugins.md) for placeholders, data directories, collisions, precedence, and security details.

## MCP

MCP servers can come from configured Agent Plugins, an explicit JSON file supplied
with `--mcp-config` or `mcp_config`, or both. Kit does not scan for other MCP
configuration automatically, and plugin-only operation needs no MCP JSON file. An
explicit file uses this format:

```json
{
  "mcpServers": {
    "local": { "command": "my-mcp-server", "args": ["--stdio"] },
    "linear": {
      "url": "https://mcp.example.com/mcp",
      "description": "Issues and project management"
    }
  }
}
```

```sh
cargo run -- tui --root /path/to/project --mcp-config /path/to/mcp.json
```

Kit validates and registers the combined MCP configuration at startup and begins
connecting every server in the background with stored credentials only. Interactive
OAuth never starts implicitly. Kit reloads an explicit file before each `tool_search`
and `auth` call; searches wait for newly added or changed servers to settle before
ranking connected tools globally. Explicit entries override same-named plugin entries.
Removing an override restores the plugin server, and duplicate supported server names
across plugins are startup errors. Descriptions should name concrete capabilities and
likely search terms. The exact query `mcp` returns a compact server-status list without
tool schemas and reports if the 32 KiB response cap omits tail entries.
Protected remote servers report `authentication_required` from their HTTP Bearer
challenge; this applies to plugin and config-declared Streamable HTTP servers.
Explicit OAuth configuration is optional and only supplies client or scope overrides.
The agent calls `auth` only when needed and gives the returned URL to the user.
Completing that browser flow updates the catalog and notifies the originating ACP
session, which resumes automatically without requiring a manual follow-up search.
Interactive OAuth is available in `tui`, `serve`, and `acp`, but not the one-shot
`prompt` command. Static `bearerToken` and `Authorization` headers remain
authoritative and are never silently replaced with inferred OAuth.

## Compose ordering

Runlet schedules independent top-level calls concurrently, including effectful
tool and `subagent` calls. Source order does not sequence them. An ordinary data
reference creates a dependency, but when later work must wait for a prerequisite
whose value it does not consume, express that control ordering explicitly with
`after prerequisite { return tool(...) }`:

```text
prepared = shell({ command: "./prepare-workspace" })
published = after prepared {
  return shell({ command: "./publish-workspace" })
}
return published
```

Calls lexically created inside the `after` block start only after the prerequisite
succeeds. Without a data dependency or `after` edge, adjacent tool or subagent
calls may overlap.

## Background compose calls

The model can detach an entire compose program from its turn with the optional
top-level `background` argument. `background: true` starts it in the background;
`background: 60` runs it in the foreground for up to 60 seconds and then detaches
it if it is still running. `false` or omission keeps normal foreground behavior.
The delay must be an integer from 1 through 86,400 seconds.

Press `Command+B` in the TUI to move the newest running foreground top-level
compose call into the background. This shortcut requires a terminal that reports
the Command key through the Kitty keyboard protocol; there is no control-key equivalent. Detached calls
remain visible and selectable in the TUI runtime graph. Interrupting the originating
turn does not stop them. The model receives each detached call's ID
and can cancel it with `close({ call_id: "call_..." })`; the selected running
background call can also be killed with `Ctrl+K` in the TUI. Completion and
cancellation are delivered through the normal background-result lifecycle. Detached
calls are scoped to the live Kit session/process; closing it does not create durable
external jobs.

## Reusable subagents

Nested agents are parent-owned named ACP subprocesses. They are ordinary Runlet
values rather than background tasks:

```text
first = subagent({ prompt: "Inspect the parser" })
second = prompt({ subagent: first, prompt: "Now propose the smallest fix" })
branch = fork({ subagent: second, prompt: "Try the alternative design" })
active = subagents({})
closed = close({ id: branch.id })
return { main: second.output, alternative: branch.output, active, closed }
```

The optional `harness` argument overrides the user's configured harness preference
with the selected value. Default to omitting it.

`subagents({})` lists the current reusable handles, including idle completed sessions, and reports an initial turn still in progress as `{ id, status: "starting" }`. Starting entries can be closed by ID but cannot be prompted or forked until the initial `subagent` call returns a complete handle. `close(handle)` or `close({ id: handle.id })` explicitly terminates one. Closing removes the handle from the parent registry and releases its capacity. For native-fork siblings sharing a process, the harness must advertise ACP `session/close`; otherwise Kit reports that it cannot close only one sibling. All retained subagents are terminated when their owning parent session closes. A parent session can retain at most 120 subagents.

Each turn-producing call returns `{ id, output, generation }`. By default, `output` remains the
agent's text. When a turn also emits non-text agent content, tool calls, tool-call
updates, or plans, the value gains an optional `updates` field containing the
ordered ACP update objects and a `truncated` flag. Text-only turns keep the
existing three-field shape. Capture is limited to 64 updates and 64 KiB of
serialized update data per turn; an oversized or excess update is omitted and
sets `truncated` to `true`. Agent thoughts, user-message echoes, usage, modes,
commands, configuration, and session metadata are not exposed. To require
structured `output`, pass an `output_schema` JSON Schema to any turn-producing
call:

```text
review = subagent({
  harness: "acp.claude",
  prompt: "Review the proposed change",
  output_schema: {
    type: "object",
    properties: {
      approved: { type: "boolean" },
      reason: { type: "string" }
    },
    required: ["approved", "reason"],
    additionalProperties: false
  }
})
return if review.output.approved {
  return { reason: review.output.reason }
} else {
  return fail("REJECTED", review.output.reason)
}
```

Kit appends the schema to the child prompt, requires the response to be a bare
JSON value, parses it, and validates it before returning the turn. Invalid schemas
are rejected before dispatch. If a completed response is malformed or does not
match the schema, its raw text is returned as `output` and the session generation
still advances, allowing a repair prompt in the same session. The schema may be
changed on a later `prompt` or `fork`. In the current pinned Runlet version,
`output` is dynamically typed: its
fields can be used in control flow as above, but Runlet does not statically check
field names or types against the per-call schema. Runtime validation determines
whether `output` is the parsed value or the raw-text fallback.

The optional `harness` is used only by `subagent`; `prompt` and `fork` infer the original profile from the
parent-owned session. Parent IDs remain distinct from ACP child session IDs.
`fork` uses native `session/fork` only when initialization advertised that
capability; sibling fork sessions can prompt concurrently while each individual
session remains serialized. Otherwise the `acp.kit` profile creates an isolated durable transcript/process
fallback, whether its executable/base argv is implicit or configured; other profiles return an explicit
unsupported error. A stale generation is rejected, which makes concurrent reuse
explicit in the Runlet dataflow. The pinned Runlet/AgentKit bridge exposes
one JSON argument per hidden tool, so `prompt` and `fork` use the small object
form above instead of two positional arguments. Built-in Kit children inherit
root, provider, model, explicit MCP configuration, credential storage, cancellation,
and nesting depth, and reload configured plugins from the same global Kit config.
Generic profiles do not inherit Kit plugin declarations or MCP configuration; they
receive standard ACP initialization/new-session/prompt
traffic and run with the root as cwd. Sessions remain reusable only for the
lifetime of the parent Kit process. Built-in Kit transcripts remain on disk
afterward; persistence for generic harnesses is agent-defined. A max-token stop
returns the partial output as a successful, reusable completed turn; cancellation,
refusal, protocol errors, and other uncertain stops retire the child instead. Permission requests from a
headless child are conservatively cancelled so they cannot hang. Nested built-in
children use `kit acp` and never start A2A listeners.

OpenAI, OpenRouter, Speakeasy, and MCP use one shared credential backend, selected with
`--credential-store` or `credential_store`. The default `memory` backend is
process-local, so credentials disappear at exit and are not available to the
TUI server process or nested Kit children. Use persistent storage when those
processes need the same credentials. Standalone provider login rejects `memory`:

```sh
# Operating-system credential store
kit auth login openai --credential-store keychain
kit auth login speakeasy --credential-store keychain
kit tui --mcp-config mcp.json --credential-store keychain

# Plain JSON files in an explicit private directory
kit auth login openai --credential-store file \
  --credential-dir ~/.local/share/kit/credentials
kit tui --mcp-config mcp.json --credential-store file \
  --credential-dir ~/.local/share/kit/credentials
```

The file backend creates a `0700` directory and `0600` credential files on
Unix, rejects unsafe paths and permissions, and does not encrypt tokens. Persistent OpenAI and Speakeasy credentials are restored when Kit starts. MCP credentials are
restored and refreshed when a matching search initializes the server, so `prompt`
can use credentials created earlier by
`tui` or `serve` without starting an interactive browser flow. MCP refreshes are
serialized across processes sharing that backend, with credentials reloaded only
after the lock is held so rotating refresh tokens are not reused.

### Signing on macOS

Build and sign the final release binary with a stable certificate and identifier
before using Keychain storage:

```sh
scripts/sign-release.sh

target/release/kit tui --mcp-config mcp.json \
  --credential-store keychain
```

On macOS, `.cargo/config.toml` routes `cargo run` through a runner that signs the
fresh debug binary before executing it. Set `KIT_CODESIGN_IDENTITY`, or put the
certificate name in the gitignored `.kit-codesign-identity` file. Both paths use
the stable identifier `com.speakeasy.kit`, overridable with
`KIT_CODESIGN_IDENTIFIER`. A changed identity, a missing certificate, or a
locked Keychain may prompt again. `cargo install`
does not run the runner; sign its installed binary separately. Apple Development
certificates are suitable for local use. Public distribution and notarization
require a Developer ID Application certificate.

## Terminal client

`tui` starts a `kit serve` child and drives it over ACP, so the client sees the
same protocol any editor would.

While a `compose` call runs, the right-hand pane draws its runtime graph: the
Runlet program parsed into its call, loop, branch, and boundary structure, with
each nested shell, edit, subagent, prompt, fork, subagents, close, A2A, and MCP meta-tool dispatch shown live under the node
that most likely issued it, including concurrent fan-out and failures. Child
call lifecycle is exact; when the same tool appears in several places, node
attribution is a stable heuristic. Nested-call
lifecycle reaches the client on stderr as marked JSON lines, enabled for the
child process with `KIT_RUNTIME_EVENTS=1`; other ACP hosts never see them.

| Key | Action |
| --- | --- |
| `⏎` | send |
| `/new` | start a fresh persisted session (`/new prompt` sends its first prompt) |
| `/sessions` | choose a workspace session and resume it |
| `/compact` | compact context now (`/compact prompt` starts the next turn with `prompt`) |
| `/effort` | choose `default`, `low`, `medium`, or `high`; Tab toggles saving the default |
| `/model` | open the searchable provider-grouped model picker |
| `/model name` | switch immediately to the closest catalog match |
| `⇧⏎`, `⌥⏎`, `^j` | newline |
| `esc` | interrupt the running turn |
| `⌘b` | move the newest running foreground compose call to the background |
| `^c` | interrupt, or quit when idle |
| `⌥←/→`, `^a`/`^e`, `home`/`end` | word and line movement |
| `⌥⌫`, `^w` | delete the previous word |
| `⌘⌫`, `^u` / `^k` | delete to line start / end |
| `↑`/`↓` | move between prompt lines, then browse history |
| `⇧↑`/`⇧↓`, `pgup`/`pgdn`, wheel | scroll the transcript |
| `^g` / `^l` / `^t` | runtime graph / agent log / reasoning |
| click a tool card, `^o` | fold its raw output open or shut |

`⌘` and `⇧⏎` need a terminal that speaks the Kitty keyboard protocol (Ghostty,
Kitty, WezTerm, recent iTerm2); the control-key equivalents work everywhere.

If the agent dies before the session opens — a taken A2A port, a missing root,
no credentials — the client exits with that agent's own last diagnostic rather
than waiting on a handshake that will never finish.

`/new` clears the visible transcript but leaves the prior persisted session
intact and resumable. `/sessions` opens the newest-first workspace catalog; Enter resumes the selected session through the same path as `/resume <session-id>`. `/model` switches the main agent and compactor in the same
live ACP session at the next safe turn boundary; it does not rewrite transcript
history or restart the child process. The picker groups models by provider and
ranks fuzzy matches as you type. Press `tab` before confirming to also replace the `provider`
and `model` defaults in the active user's `~/.kit/config.toml`; this safely
rewrites the TOML but does not preserve its formatting or comments. Otherwise, the
choice lasts only for this live ACP session. Catalog discovery is bounded and
falls back to a small built-in list when a provider catalog is unavailable.
`/compact` ends after compaction when used alone; text after
the command is retained as a new user message and starts the next model turn.
Slash commands are recognized only as exact leading
tokens. Unknown slash commands are sent to the model unchanged. Pasting
multi-line text puts line breaks in the prompt instead of sending it.
The client asks the terminal to bracket pastes; where that is unavailable a
paste arrives as a key burst, and a return inside such a burst is read as a
line break rather than a send. Either way the whole paste is applied in one
redraw.

Dragging local PNG, JPEG, GIF, WebP, WAV, or MP3 files into the terminal adds
them as `[Image #N]` or `[Audio #N]` prompt attachments. Kit recognizes a drop
only when every pasted token is a supported file; otherwise it preserves the
whole paste as text. A prompt can contain at most 8 attachments, 10 MiB each
and 20 MiB total. Kit sends the bytes through OpenRouter or OpenAI subscription
while retaining canonical local `file://` links in model-facing text. Model
modality support varies. In terminals detected as supporting Kitty, Sixel, or
iTerm2 graphics, user-attached images render inline as a bounded static first
frame; other terminals and decode failures keep the safe clickable attachment
label. Video and audio playback are not supported, and Kit never displays
base64 or `data:` URLs. See
[the TUI guide](docs/user/tui-and-sessions.md#attach-local-images-and-audio).

## Deliberate limits

The root is a working directory, not a sandbox. Shell commands can access the
host. Edits are atomic per file but multi-file changes are not transactional.
Subagents are child ACP processes, use the same root as their working directory,
and are bounded to depth two.

## Reporting Kit issues

Report bugs and feature requests in the [Kit issue tracker](https://github.com/speakeasy-api/kit/issues). Before opening an issue, follow the [reporting guide](docs/user/reporting-kit-issues.md) to include the Kit version and useful reproduction details. Agents must get the user's explicit permission before submitting an issue on their behalf.
