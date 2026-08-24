# Security, Limits, and Troubleshooting

Kit is a directory-rooted coding-agent runtime, not a security boundary. Treat the model, prompts, configured ACP harnesses, MCP servers, and remote A2A agents according to the access they receive. For command-specific options, use `kit --help` and `kit <command> --help`; this page describes behavior and boundaries rather than every CLI flag.

## Trust model and runtime root

The runtime root is the working directory and project context, **not a sandbox**. Kit canonicalizes the root at startup and rejects a missing root (`could not open runtime root`) or a non-directory (`runtime root is not a directory`). An ACP client must open exactly that root and cannot add `additional_directories`.

The root does not confine processes:

- `shell` starts the platform shell with the root as its current directory. The command can read or change anything allowed by the Kit process, including paths outside the root, the network, and inherited environment variables.
- ACP subagents are child processes started directly from trusted local `command` and `args` profiles, with the same root as their current directory. They inherit normal child-process host access; selecting a harness is not isolation.
- MCP stdio servers are local processes, while MCP HTTP servers and A2A agents are remote trust domains. Tool arguments, prompts, and returned data cross those boundaries.
- `edit` resolves relative paths from the runtime root, but also accepts `..`, absolute paths, and paths through symlinks. It can change any filesystem path allowed by the Kit process.

Kit has no general permissions or interactive approval framework for its own tools. The fact that only `compose` is model-visible organizes tool use; it does not make hidden `shell`, `edit`, subagent, A2A, or MCP calls harmless. Review requested work and run Kit with the least host, repository, network, and account access appropriate for it.

## A2A network exposure and remote-agent trust

`kit serve` always starts ACP on stdio and an HTTP listener. A2A is enabled by default; `--remote-acp` adds ACP at `/acp`, and `--no-a2a --remote-acp` exposes only remote ACP. When no address is configured, Kit binds an available loopback address (`127.0.0.1:0`). Use `kit acp` when no HTTP listener is wanted.

The A2A server advertises no `security_schemes` or `security_requirements` and performs no authentication or authorization in Kit. Every client that can reach the listener can submit text that runs a fresh Kit agent with the configured root and tools. Consequently:

- Keep the default loopback binding unless remote access is deliberately required.
- Binding `0.0.0.0`, a LAN address, or another non-loopback interface exposes a coding agent, not just a read-only status endpoint. Put external authentication and network controls in front of it if exposure is intentional.
- A fixed port can fail because it is already in use. An omitted port avoids selecting a fixed number, but it does not add authentication.

The `a2a` tool is an outbound client. It sends the supplied prompt as plain A2A text to the supplied URL and returns the remote task or message. Kit does not add credentials, restrict destinations, or impose a request timeout. Do not send repository secrets, credentials, private code, or untrusted instructions to an A2A URL merely because another agent supplied it. Interrupting the turn cancels Kit's wait, but cannot retract a prompt already delivered or undo remote side effects.

Example safe local-only server startup:

```sh
kit serve --root /path/to/project --http 127.0.0.1:7331
kit serve --root /path/to/project --remote-acp --server-credential-file /private/token
```

Confirm current options with `kit serve --help`.

## Permission requests and their limitations

A generic nested ACP agent runs headlessly, so Kit cannot ask a human to approve a permission request. Its trusted profile uses `permissions = "deny"` by default: Kit selects `reject_always`, falling back to `reject_once`, when the child offers one. If no reject option exists, Kit cancels the request. `permissions = "cancel"` always cancels it. Kit never selects an allow option.

This is fail-closed handling of **ACP permission requests only**. It is not a policy engine for filesystem paths, shell commands, network calls, inherited credentials, MCP tools, or A2A. A child that performs an operation without requesting ACP permission remains governed only by its process privileges and its own implementation. Configure ACP profiles only for executables and fixed arguments you trust.

Typical symptoms are a nested task that stops at an approval boundary, `nested agent cancelled`, or `nested agent refused the prompt`. Change the task or the child agent's own setup rather than assuming Kit can grant an interactive approval.

## Side effects, concurrency, interruption, and persistence

Tool calls are real operations. Reads may trigger remote service behavior, and writes, shell commands, MCP calls, ACP agents, and A2A agents may change local or remote state. In a Runlet, independent calls and loop iterations run concurrently; source order alone is not sequencing. Use data dependencies or `after` when one operation must wait for another. A later failure does not roll back calls that already ran.

`edit` writes additions and replacements through a temporary file and rename, so each successful file write is atomic. Deletes are direct file removals. A set of edits across files is not transactional, and neither Kit nor interruption provides rollback. Exact hunk anchors must match once; `hunk anchor did not match` and `hunk anchor is ambiguous` are safeguards, not partial edits.

Interrupting a turn cancels waiting work and Kit attempts to kill an active shell or ACP child. It cannot reverse a completed filesystem write, command, network request, or remote tool action. Inspect the working tree and external systems after cancellation.

Persisted session transcripts are append-only JSONL records under `~/.kit/sessions` by default. They can contain prompts and tool traffic, so avoid putting secrets in prompts or tool arguments and protect the session directory. A filesystem lock prevents two live Kit processes from mutating the same session. `--force` fences a stale lock; it must not be used to steal a session from a live owner.

## Credentials and secret-handling risks

Kit can expose credentials through the authority of its process even when it does not print them. Shell commands and ACP harnesses normally inherit the process environment. Local MCP servers, including plugin stdio servers, are executable code. Plugin stdio servers receive absolute `PLUGIN_ROOT` and `PLUGIN_DATA` paths and can read or modify anything allowed by the Kit process. Remote MCP tools receive invoked arguments, and explicit MCP configuration may contain a plain `bearerToken`. Keep config files and environment variables private, use narrowly scoped accounts, and do not ask the model to echo tokens for diagnosis.

OpenAI and MCP use one credential backend selected with `--credential-store` or `credential_store`. The default is `memory`. `kit auth login openai` uses PKCE, state, and nonce on fixed loopback callback ports 1455 and 1457 and verifies RS256 tokens against OpenAI's pinned JWKS endpoint. Standalone OpenAI login rejects `memory`; select persistent `keychain` or `file` storage and use the same selection for runtime commands. Token values are redacted from diagnostics and zeroized where practical. The synchronization lock file contains no credentials. `kit auth logout openai` revokes before deletion and retains the credential when revocation fails; `--local-only` deliberately skips revocation and should be used only when remote revocation cannot be completed.

The shared backend choices are:

- `memory`: not persisted. It is process-local, so credentials are not available to the TUI server process or nested Kit children.
- `keychain`: persistent operating-system storage through macOS Keychain, Windows Credential Manager, or Secret Service on Linux and other Unix systems.
- `file`: persistent but **not encrypted**. It requires `--credential-dir` or `credential_dir`. On Unix, Kit creates the credential directory with mode `0700` and files with mode `0600`, rejects symlinked storage directories and non-regular paths, and rejects credential files accessible by other users.

Interactive OAuth is available in `kit tui`, `kit serve`, and the stdio-only `kit acp` command. The one-shot `kit prompt` command preserves a remote server's `authentication_required` status but reports `interactive MCP authentication requires the tui, serve, or acp command` if `auth` is called; it can still restore previously persisted credentials. An OAuth flow binds a loopback callback, expires after 10 minutes, and may report `OAuth callback timed out`.

A typical login flow is to start a long-lived client, let the agent use `tool_search`, and open the URL returned by `auth`. After the browser callback completes, Kit connects the server and automatically resumes the originating ACP session:

```sh
kit tui --root /path/to/project --mcp-config /path/to/mcp.json
```

Use `kit tui --help` for credential-store options. If nested Kit agents need the same authenticated MCP account, choose persistent storage deliberately and account for the increased credential exposure.

## Operational limits and expected errors

The following are fixed runtime limits, not configurable policy controls:

- Shell timeout: 120 seconds by default; accepted values are 1 through 3600 seconds. Timeout reports `shell command timed out`. Shell stdout and stderr remain complete inside compose and fail if either stream exceeds the 64 MiB internal safety limit. Final compose results from 8 KiB through the 64 MiB result limit spill at the model-context boundary, which receives a bounded head-and-tail preview and artifact path.
- Subagents: nesting depth is two and at most 120 live subagent sessions are retained per main session. Errors include `subagent depth limit (2) reached` and `live subagent session limit (120) reached`. Reuse completed sessions or release unneeded ones with `close` instead of creating unbounded children.
- ACP children: startup handshake and `session/fork` waits are 30 seconds. Cancellation allows 5 seconds to settle before Kit tears down the child. Captured ACP updates are limited to 64 updates and 64 KiB; the returned `updates.truncated` flag reports loss.
- MCP: background server initialization uses a 20-second connection timeout. Tool calls have a 60-second deadline by default; `timeout_seconds` can override it with a value from 1 through 3600 for a call expected to take longer. OAuth authorization expires after 10 minutes. `tool_search` returns at most 5 tools globally across all servers and caps the serialized response at 32 KiB; search with a configured server name, specific product term, or tool keywords. Use the exact query `mcp` (case-insensitive) for a compact configured-server status list; `total_servers`, `returned_servers`, and `truncated` report any tail entries omitted by the same cap.
- A2A outbound calls have no Kit request deadline, but remain interruptible. Inbound A2A requests must contain a text part or fail with `A2A request must contain a text part`.
- Session IDs are 1–128 ASCII letters, digits, `-`, or `_`. A stale subagent value fails with `stale subagent generation ...`; always pass the latest returned value to `prompt` or `fork`. An uncertain failed subagent turn retires that child because it may already have changed durable state.

Provider context windows, model token limits, child-agent turn limits, remote rate limits, and operating-system resource limits are separate. For example, `nested agent reached its turn-request limit` comes from the child ACP stop reason, not the 120-session capacity limit.

## Troubleshooting decision tree

### Kit does not start or the TUI exits before opening a session

1. Run `kit --version` and `kit <command> --help` to verify the installed binary and command syntax.
2. If the diagnostic says `could not open runtime root` or `runtime root is not a directory`, verify that `--root` exists, is a directory, and is accessible to the Kit process.
3. If A2A binding fails or the port is taken, omit the fixed address to get an available loopback port, choose another loopback port, or use `kit acp` when HTTP is unnecessary.
4. If the failure mentions OpenAI subscription credentials, run status and login with the same persistent `--credential-store keychain` or `--credential-store file --credential-dir ...` used by the runtime; standalone login rejects `memory`. Ensure loopback port 1455 or 1457 is available. Retry without pasting secrets into the prompt.

### A shell or edit tool fails

1. For `shell command timed out`, reduce the task, diagnose a blocked subprocess, or intentionally raise `timeout_seconds` within 1–3600. An interrupt kills Kit's child command but still inspect for partial side effects.
2. When a result reports `compose output spilled`, inspect a small relevant range from its artifact instead of returning the full artifact to model context. Internal Runlet consumers receive complete shell output before this boundary guard runs.
3. For `path must be non-empty`, provide either a path relative to the runtime root or an absolute path.
4. For a hunk mismatch or ambiguity, reread the file and provide more unique exact context.

### A subagent fails, hangs, or cannot fork

1. `unknown ACP harness` means the requested `acp.<name>` is not a configured trusted profile. Check local configuration and omit the `harness` override to use the configured default.
2. `ACP harness spawn failure` means the configured executable could not start; verify it is installed and executable. `ACP harness handshake timeout` or `protocol handshake failure` means it did not complete ACP v1 startup within 30 seconds. Child-controlled startup errors are intentionally replaced with a fixed local diagnostic to avoid leaking secrets.
3. A cancelled permission request is expected for children that require interactive approval; Kit's headless policy never allows it.
4. For a depth or live-session limit, inspect retained handles with `subagents({})`, reuse `prompt`, close unneeded children with `close`, or simplify fan-out.
5. `ACP harness does not support session/fork` means the generic harness did not advertise native fork; transcript fallback exists only for `acp.kit`.
6. For `stale subagent generation`, use the newest session value. After an unsuccessful dispatched continuation, start a new child because the old session is retired.

### MCP tools are missing or authentication fails

1. Confirm that the server comes from a configured, valid Agent Plugin or from the selected explicit MCP file. Plugin-only operation needs no file, but Kit does not scan for unconfigured plugins or other MCP files. An SSE plugin server is skipped with a diagnostic; use `streamable-http` instead. A duplicate supported server name across two plugins stops startup, while an explicit same-named entry intentionally overrides a plugin server.
2. Search with `tool_search`; it waits for background server initialization to settle, and the exact query `mcp` compactly lists configured servers and reports any cap-driven tail omission. Inspect statuses: `authenticated`, `authentication_required`, `pending`, or `error`. An `error` result includes the initialization diagnostic. Invoke only tool names returned by the search.
3. For `authentication_required`, call `auth` with the exact server name in a `kit tui`, `kit serve`, or `kit acp` session, open its URL, and complete the browser flow within 10 minutes. Kit connects the server and resumes the originating ACP session automatically. If a configured `bearerToken` or `Authorization` header is rejected, Kit reports an error instead and does not replace that static credential with inferred OAuth; update or remove it first.
4. In one-shot `kit prompt`, use a long-lived command to authenticate or configure persistent credentials there for later one-shot use.
5. For file-store errors such as `OAuth credential directory must be a real directory, not a symlink`, `OAuth credential path must be a regular file`, or `OAuth credential file is accessible by other users`, correct ownership, path type, and permissions rather than weakening the checks.
6. If an explicit override was removed, the plugin server with that name is restored on the next `tool_search` or `auth`. If the file is invalid, repair it first; Kit retains the last valid combined configuration.
7. If a server has status `error`, relay its diagnostic to the user, test the configured stdio command or remote URL independently, and check the 20-second connection limit. Treat server diagnostics as potentially sensitive.

### A session cannot be resumed

1. `session ... does not exist` means the selected ID has no durable transcript. Check the ID shown by the original Kit process.
2. `session is actively locked by another Kit instance` means the owner is live; stop or use that instance. Do not use `--force`.
3. `use --force to override a stale lock` applies only after confirming the prior process is gone. Then retry the same installed command with its documented force option.
4. `invalid transcript line`, `unsupported session schema version`, or an identity/generation error indicates corrupt or incompatible persisted data. Preserve the file for diagnosis, avoid hand-editing it in place, and start a new session if recovery is not possible.
