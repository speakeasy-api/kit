# Configure and Use MCP Servers

Kit merges Model Context Protocol (MCP) servers from Agent Plugins, `mcp_config` in `~/.kit/config.toml`, `.mcp.json` in the canonical runtime root, and `--mcp-config`, in that precedence order. A higher layer replaces a whole same-named server; non-conflicting lower-layer servers remain. The project file is optional, while configured and command-line files are required when specified. Relative configured and command-line paths retain launch-directory resolution. Run `kit --help` and `kit <command> --help` for the exhaustive CLI reference.

## Ask the agent to add a server

Kit does not provide an MCP-specific slash command. In a `kit tui` session, describe the server and the scope that you want:

> Add the Linear MCP server at `https://mcp.linear.app/mcp` for this project. Preserve the existing entries in `.mcp.json`, add a description that will help you find its tools, validate the file, and help me authenticate.

For a server that should be available in every project, ask the agent to update the file selected by `mcp_config` in `~/.kit/config.toml` instead. The agent should inspect the existing file before editing it, preserve unrelated servers, and never put a credential in the file unless you explicitly require static secret configuration. You can ask it to show the compact MCP status listing after the edit. New and changed servers load on the next `tool_search` or `auth` call without a restart.

Files are not the only installation route. An Agent Plugin can contribute MCP servers, and `--mcp-config` can add an explicit JSON layer for one launch. The rest of this guide documents each route and the strict JSON format for manual edits.

## Agent Plugin MCP configuration

Validated Agent Plugins can contribute `stdio` and `streamable-http` servers. Deprecated `sse` servers are skipped with a stderr diagnostic naming the plugin and server. Supported server names must be unique across plugins; a collision stops startup and identifies both plugin aliases.

Plugin stdio declarations use the canonical plugin package as `PLUGIN_ROOT` and a persistent `<Kit-config-directory>/plugin-data/<plugin-manifest-name>` directory as `PLUGIN_DATA`. Kit injects both environment variables and replaces every occurrence of `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` in stdio arguments and plugin-supplied environment values. A `./` command is resolved beneath the plugin root. An omitted `cwd` leaves the transport default unchanged and inherits Kit's working directory; `./...` is plugin-root-relative; and `${PLUGIN_ROOT}` or `${PLUGIN_DATA}`, optionally with a validated contained suffix, selects that directory. Kit creates missing data-rooted working directories. These stdio placeholders are not expanded in streamable HTTP URLs or headers.

Higher-precedence JSON entries override same-named plugin servers. Kit reloads every named JSON layer before each `tool_search` and `auth`; removing an override restores the next lower layer in the live runtime. Creating or deleting the optional project file is detected the same way. An invalid edit fails that call while retaining the last valid combined configuration. See [Agent Plugins](agent-plugins.md) for package configuration and ACP inheritance details.

## MCP JSON configuration

The top-level key is `mcpServers`; each key beneath it is the server name shown by `tool_search` and accepted by `auth`. The JSON schema is strict: unknown fields, mixed `command`/`url` transports, and mismatched transport types are invalid. The optional `type` is `stdio` for a command server and `streamable-http` or `http` for a URL server; legacy entries without `type` remain valid. Kit validates every named layer before changing runtime state, then reloads them before each `tool_search` and `auth` call so live sessions see added, changed, and removed servers—and restored lower layers—without a restart. An invalid edit makes the current call fail but retains the last valid combined configuration so it becomes usable again when the file is repaired. Every configured server begins connecting in the background when Kit starts, using stored credentials only; interactive OAuth is never started implicitly. Each `tool_search` call waits for servers that are still initializing—including servers just added by a reload—before searching, so results always reflect a settled configuration. Give every server a specific `description`: it appears in listings and search results, and it is how an agent recognizes a server that still needs authentication. Use the exact query `mcp` (case-insensitive) for a compact configured-server status list. If the response cap omits tail entries, `total_servers`, `returned_servers`, and `truncated` report the omission.

```json
{
  "mcpServers": {
    "local-files": {
      "type": "stdio",
      "command": "my-mcp-server",
      "args": ["--stdio"],
      "cwd": "/path/to/project",
      "env": { "LOG_LEVEL": "warn" },
      "description": "Local file tools"
    },
    "projects": {
      "type": "streamable-http",
      "url": "https://mcp.example.com/mcp",
      "description": "Issues and project management",
      "auth": {
        "type": "oauth",
        "scopes": ["issues:read"]
      }
    }
  }
}
```

For a project-local stdio server, an omitted `cwd` defaults to the directory containing the root `.mcp.json`, and a relative `cwd` resolves from that directory. An absolute `cwd` is unchanged. Other layers preserve the existing transport behavior. Kit reads these files but never rewrites them.

Start Kit with the installed binary:

```sh
kit tui --root /path/to/project --mcp-config /path/to/mcp.json
```

The equivalent home configuration is:

```toml
mcp_config = "/path/to/mcp.json"
```

Server names must have unambiguous tool prefixes. Do not configure names where one name plus `_` prefixes another, such as `foo` and `foo_bar`; Kit rejects that configuration because both can produce the same model-visible MCP tool name.

### Local stdio transport

A stdio server requires a non-empty `command`. Optional fields are `args` (an array of strings), `env` (a string-to-string object), `cwd`, and `description`. Kit starts and connects a configured stdio server in the background at startup, or as soon as a reload adds it and the next `tool_search` or `auth` call initializes it. Treat `command`, `args`, `cwd`, and environment values as executable configuration, and review the file before using it.

### Remote Streamable HTTP transport

An HTTP server requires a non-empty `url`. Optional fields are `description`, `headers`, `bearerToken`, and `auth`. For a static token or custom headers:

```json
{
  "mcpServers": {
    "internal": {
      "url": "https://mcp.example.com/mcp",
      "bearerToken": "replace-with-token",
      "headers": { "X-Tenant": "engineering" }
    }
  }
}
```

`bearerToken` and header values are plaintext secrets in the MCP JSON file. Restrict access to that file and prefer HTTPS. A `bearerToken` or case-insensitive `Authorization` header is authoritative static authorization: Kit does not replace it with inferred OAuth if the server rejects it. Update or remove that credential before calling `auth`. A server cannot combine an `auth` block with static authorization; startup reports `cannot use both OAuth and static authorization`.

### Optional OAuth configuration fields

OAuth is inferred reactively for every remote Streamable HTTP server. Kit first connects normally, records a `WWW-Authenticate` Bearer challenge, and uses that challenge for protected-resource discovery and dynamic client registration when `auth` is called. No `auth` block is required. Stdio servers never use this OAuth path.

An explicit `auth` block is an optional fallback or override. Set `auth.type` to the exact value `oauth`. `scopes` supplies fallback scopes in addition to challenge and server metadata. Use `clientId` to override dynamic registration with a pre-registered OAuth client. Optional `clientMetadataUrl` supplies a client metadata document during registration.

```json
{
  "mcpServers": {
    "remote": {
      "url": "https://mcp.example.com/mcp",
      "auth": {
        "type": "oauth",
        "clientId": "kit-client-id",
        "scopes": ["read", "write"]
      }
    }
  }
}
```

## Discover, authenticate, and call MCP tools

MCP tools are model-visible through three meta-tools rather than as an unrestricted static list. The expected workflow is:

1. Call `tool_search({ query: "issues" })`. The search reloads the configuration, waits for any servers that are still initializing, then ranks every connected server's tools globally with high precision: a tool must match the query in its name or match every query term. At most 5 tools are returned across all servers—often fewer—sorted by score and grouped by server with `available_tool_count`, `matched_tool_count`, `returned_tool_count`, and `truncated`, plus top-level `total_matched`, `total_returned`, and `truncated`. Responses are capped at 32 KiB by dropping lowest-ranked status-only server groups before lowest-ranked tools; the counts show when that happened. A server whose name strongly matches the query but which needs authentication or failed to connect is listed without tools. Use the exact query `tool_search({ query: "mcp" })` (case-insensitive) for a compact configured-server listing—name, bounded description, status, bounded optional error, and `available_tool_count` (`null` unless connected)—without any tool schemas. Compact listings drop tail servers when necessary and report `total_servers`, `returned_servers`, and `truncated`.
2. If the selected server has status `authentication_required`, call `auth({ name: "projects" })` with its exact server name. Give the returned `url` to the user. While the flow is active, status is `pending`; the loopback browser callback expires after 10 minutes.
3. When the user completes OAuth, Kit stores the credentials, connects the server, and sends a notification to the originating ACP session. The agent resumes automatically and can search or call the newly available tools; no manual post-authentication search is required.
4. Invoke only a returned MCP tool name: `tool({ name: "returned_tool_name", args: { ... } })`. `args` must be an object matching that tool's advertised input schema. Calls have a 60-second deadline by default. Set the optional `timeout_seconds` field from 1 through 3600 only when the tool is expected to return after that default; for example, `tool({ name: "returned_tool_name", args: { ... }, timeout_seconds: 300 })`.

A connected server reports `authenticated`, including servers that do not need OAuth. If an already-connected OAuth server rejects a tool call with a Bearer challenge, Kit first refreshes the existing credentials and replays that tool call once. A missing or failed refresh, a request for additional scopes, or a rejected replay falls back to the explicit `auth` workflow. A remote server whose HTTP response requests Bearer authentication reports `authentication_required`, whether it came from a plugin or explicit configuration and whether it has an `auth` block. Other initialization failures report `error` with a diagnostic. Calling `auth` for an unknown name reports `unknown MCP server`; calling it for a stdio server reports `is not remote`. Calling an undiscovered or unavailable tool reports `unknown MCP tool`.

## Interactive OAuth availability

Interactive browser authentication is enabled in the long-lived `kit tui`, `kit serve`, and `kit acp` runtimes. It is disabled in the one-shot `kit prompt` command. A challenged server still appears as `authentication_required` so the cause is preserved, while calling `auth` reports `interactive MCP authentication requires the tui, serve, or acp command`.

`kit prompt` can still use OAuth credentials restored from a persistent store. Authenticate first in a long-lived runtime using the same MCP configuration and credential store, then run the prompt with those settings. The OAuth redirect listener binds a temporary `127.0.0.1` port, so the browser must be able to reach the local callback.

## ACP child behavior

Nested built-in `acp.kit` children receive the configured and explicit MCP paths, credential settings, and effective project root from their parent. They rediscover the project `.mcp.json` and reload plugins from the same global Kit configuration, preserving the plugin → configured → project → explicit precedence. External ACP profiles are separate programs: Kit sends them standard ACP initialization and prompt traffic, but does not inject Kit plugin declarations or Kit MCP configuration. Configure MCP separately in an external agent if it supports that behavior.

## OAuth credential stores

OpenAI and MCP use one shared backend. Choose it with `--credential-store` or `credential_store` in `~/.kit/config.toml`. The allowed values are `memory`, `keychain`, and `file`.

### Memory store (default)

`memory` is process-local and writes no OAuth credentials to persistent storage. Credentials disappear when the process exits and are not available to the TUI server process or nested Kit children. This is the default when no store is selected. Standalone OpenAI login rejects `memory`; select `keychain` or `file` for login and reuse that backend in runtime commands.

```sh
kit tui --mcp-config /path/to/mcp.json --credential-store memory
```

### Operating-system credential store

`keychain` persists OAuth credentials in the platform credential store: macOS Keychain, Windows Credential Manager, or Secret Service on Linux and other Unix systems. The store must be available and unlocked. On macOS, a stable, signed installed binary avoids repeated Keychain identity prompts; a changed signing identity, missing certificate, or locked Keychain can require attention.

```sh
kit tui --mcp-config /path/to/mcp.json --credential-store keychain
```

### File credential store

`file` requires an explicit credential directory. Kit stores unencrypted JSON credentials under hashed filenames. On Unix, it makes the directory mode `0700`, creates files with mode `0600`, rejects a symlinked credential directory, rejects non-regular credential paths, and refuses to load files accessible by other users. These checks protect filesystem access but do not encrypt tokens; protect backups and the host account too.

```sh
kit tui --mcp-config /path/to/mcp.json \
  --credential-store file \
  --credential-dir ~/.local/share/kit/credentials
```

The corresponding TOML is:

```toml
credential_store = "file"
credential_dir = "/path/to/private/credentials"
```

Persistent stores restore OpenAI credentials when Kit starts and restore MCP OAuth credentials during the background server initialization at startup—or when a later reload adds the server and the next `tool_search` or `auth` call initializes it; either path may refresh access tokens. This also works for the inferred default OAuth identity of a remote server with no `auth` block. Reuse the same backend, directory, and MCP server identity across commands. Kit serializes MCP token refreshes across processes that share a credential backend, reloads credentials after taking the lock, and reuses a token that another waiter already refreshed so rotating refresh tokens are not consumed twice.

## Security guidance

- Use only MCP servers and executables you trust. Stdio configuration launches a local command, while remote tools send their declared arguments to an HTTP service.
- Keep MCP JSON files private when they contain `bearerToken`, custom authorization headers, client identifiers, or other sensitive values.
- Prefer OAuth or an external secret-injection strategy over committing static tokens. Do not commit file-backed OAuth credentials.
- Review tool names, descriptions, and input schemas returned by `tool_search` before calling `tool`; discovery does not make a remote action safe.
- Use a persistent credential store only when persistence is needed. The default memory store minimizes credentials left on disk.

## Troubleshooting MCP configuration and connections

### Configuration file errors

- **`could not read MCP config ...`**: verify the configured `mcp_config` or `--mcp-config` path and permissions; named files are required. Root `.mcp.json` is optional, and its absence is tracked for live creation.
- **`invalid MCP config ...`**: validate JSON syntax, the exact `mcpServers` spelling, field types, and field names such as `bearerToken`, `clientId`, and `clientMetadataUrl`. Unknown fields are rejected.
- **`MCP server names must not be empty`**, **`has an empty command`**, or **`has an empty URL`**: give every entry a non-blank name and its transport a non-blank `command` or `url`.
- **`MCP server ... is declared by both plugins ...`**: rename one plugin server or disable one of the colliding plugins. Explicit-file entries may override plugin servers, but plugin/plugin collisions are errors.
- **Plugin SSE skip diagnostic**: migrate that declaration to `streamable-http`; SSE is deprecated and Kit intentionally leaves that server unavailable.
- **MCP server status `error`**: Kit tolerates initialization failures—startup continues and the per-server diagnostic is included in `tool_search`. Run a failing stdio command directly to check that it exists and speaks MCP over stdio; for HTTP, check DNS, HTTPS, proxy/firewall access, authentication settings, headers, and the endpoint URL. The connection timeout is 20 seconds.

### OAuth and authentication errors

- **`OAuth discovery failed` / `OAuth client registration failed`**: verify the server's `WWW-Authenticate` challenge and OAuth metadata and whether it supports dynamic registration. If it requires a registered client, add an optional `auth.clientId` override.
- **`could not bind OAuth callback`**: permit Kit to bind a loopback port. **`OAuth callback timed out`** means the browser flow was not completed within 10 minutes; call `auth` again.
- **Authentication completed but the agent did not resume**: wait for the callback to finish and keep the originating ACP session open. Kit sends that session a success or failure notification after the connection attempt; failures are also written as `MCP authentication for <server> failed: ...` on stderr. A manual `tool_search` can inspect current status but is not required for the normal flow.
- **Stored credentials no longer work**: Kit automatically refreshes and replays one rejected OAuth tool call when possible. If that attempt fails or the replay is rejected, authenticate again in a long-lived runtime. Processes sharing a persistent backend serialize refreshes; if credentials were changed externally, restart or authenticate again.

### Credential-store errors

- **`credential_dir is required when credential_store is file`**: supply `--credential-dir` or the TOML key.
- **`credential_dir requires credential_store to be file`**: remove the directory setting or select `file`.
- **`OAuth credential directory must be a real directory, not a symlink`**: select a real private directory.
- **`OAuth credential path must be a regular file`**: remove the conflicting symlink or non-file entry.
- **`OAuth credential file is accessible by other users`**: on Unix, restrict the file to its owner (for example, mode `0600`) before retrying.
- **Credential-store read/write failures**: confirm the operating-system store is available and unlocked. On macOS, also confirm that the installed binary has the expected stable signing identity.
