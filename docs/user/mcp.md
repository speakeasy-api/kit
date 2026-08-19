# Configure and Use MCP Servers

Kit connects only to Model Context Protocol (MCP) servers in an explicit JSON file. It does not discover or execute MCP configuration automatically. Supply the file with `--mcp-config` or set `mcp_config` in `~/.kit/config.toml`. Command-line values override TOML values. Run `kit --help` and `kit <command> --help` for the exhaustive CLI reference.

## MCP JSON configuration

The top-level key is `mcpServers`; each key beneath it is the server name shown by `tool_search` and accepted by `auth`. The JSON schema is strict: unknown fields make the configuration invalid.

```json
{
  "mcpServers": {
    "local-files": {
      "command": "my-mcp-server",
      "args": ["--stdio"],
      "cwd": "/path/to/project",
      "env": { "LOG_LEVEL": "warn" },
      "description": "Local file tools"
    },
    "projects": {
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

Start Kit with the installed binary:

```sh
kit tui --root /path/to/project --mcp-config /path/to/mcp.json
```

The equivalent home configuration is:

```toml
mcp_config = "/path/to/mcp.json"
```

### Local stdio transport

A stdio server requires a non-empty `command`. Optional fields are `args` (an array of strings), `env` (a string-to-string object), `cwd`, and `description`. Kit starts and connects configured stdio servers when the runtime starts; treat `command`, `args`, `cwd`, and environment values as executable configuration, and review the file before using it.

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

`bearerToken` and header values are plaintext secrets in the MCP JSON file. Restrict access to that file and prefer HTTPS. A server cannot set both `auth` and `bearerToken`; startup reports `cannot use both OAuth and bearerToken`.

### OAuth configuration fields

OAuth is available only for remote HTTP servers. Set `auth.type` to the exact value `oauth`. `scopes` is an optional array. Use `clientId` for a pre-registered OAuth client; without it, Kit performs dynamic client registration. Optional `clientMetadataUrl` is supplied during dynamic registration.

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

1. Call `tool_search({ query: "issues" })`. Use `tool_search({ query: "mcp" })` to list every configured server. Results are grouped by server and include the configured name, description, status, and matching tools.
2. If the selected server has status `authentication_required`, call `auth({ name: "projects" })` with its exact server name. Give the returned `url` to the user. While the flow is active, status is `pending`; the loopback browser callback expires after 10 minutes.
3. After the user completes OAuth in the browser, call `tool_search` again so the connected server's tools appear.
4. Invoke only a returned MCP tool name: `tool({ name: "returned_tool_name", args: { ... } })`. `args` must be an object matching that tool's advertised input schema.

A connected server reports `authenticated`, including servers that do not need OAuth. If a server fails to connect during startup, Kit continues running and reports the server with status `error` and an `error` message in `tool_search`; the agent can relay that diagnostic to the user. Calling `auth` for an unknown name reports `unknown MCP server`; calling it for a server without OAuth reports `is not configured for OAuth`. Calling an undiscovered or unavailable tool reports `unknown MCP tool`.

## Interactive OAuth availability

Interactive browser authentication is enabled in the long-lived `kit tui`, `kit serve`, and `kit acp` runtimes. It is disabled in the one-shot `kit prompt` command; an unauthenticated OAuth server then appears as `authentication_unavailable`, and `auth` reports `interactive MCP authentication requires the tui, serve, or acp command`.

`kit prompt` can still use OAuth credentials restored from a persistent store. Authenticate first in a long-lived runtime using the same MCP configuration and credential store, then run the prompt with those settings. The OAuth redirect listener binds a temporary `127.0.0.1` port, so the browser must be able to reach the local callback.

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

Persistent stores restore OpenAI and MCP OAuth credentials when Kit starts and may refresh access tokens. Reuse the same backend, directory, and MCP server identity across commands. Concurrent Kit processes can race if an OAuth provider rotates refresh tokens.

## Security guidance

- Use only MCP servers and executables you trust. Stdio configuration launches a local command, while remote tools send their declared arguments to an HTTP service.
- Keep MCP JSON files private when they contain `bearerToken`, custom authorization headers, client identifiers, or other sensitive values.
- Prefer OAuth or an external secret-injection strategy over committing static tokens. Do not commit file-backed OAuth credentials.
- Review tool names, descriptions, and input schemas returned by `tool_search` before calling `tool`; discovery does not make a remote action safe.
- Use a persistent credential store only when persistence is needed. The default memory store minimizes credentials left on disk.

## Troubleshooting MCP configuration and connections

### Configuration file errors

- **`could not read MCP config ...`**: verify the `--mcp-config`/`mcp_config` path and file permissions. MCP configuration is never found automatically.
- **`invalid MCP config ...`**: validate JSON syntax, the exact `mcpServers` spelling, field types, and field names such as `bearerToken`, `clientId`, and `clientMetadataUrl`. Unknown fields are rejected.
- **`MCP server names must not be empty`**, **`has an empty command`**, or **`has an empty URL`**: give every entry a non-blank name and its transport a non-blank `command` or `url`.
- **MCP server status `error`**: Kit tolerates startup connection failures and includes the per-server diagnostic in `tool_search`. Run a failing stdio command directly to check that it exists and speaks MCP over stdio; for HTTP, check DNS, HTTPS, proxy/firewall access, authentication settings, headers, and the endpoint URL. Initial connection timeout is 20 seconds.

### OAuth and authentication errors

- **`authentication_unavailable`**: use `kit tui`, `kit serve`, or `kit acp` to complete browser OAuth, or configure a persistent store containing credentials created earlier.
- **`OAuth discovery failed` / `OAuth client registration failed`**: verify the server's OAuth metadata and whether it supports dynamic registration. If it requires a registered client, configure `clientId`.
- **`could not bind OAuth callback`**: permit Kit to bind a loopback port. **`OAuth callback timed out`** means the browser flow was not completed within 10 minutes; call `auth` again.
- **Authentication completed but tools are missing**: wait for the callback to finish, then call `tool_search` again. If authentication failed asynchronously, Kit writes `MCP authentication for <server> failed: ...` to stderr and returns the server to `authentication_required`.
- **Stored credentials no longer work**: authenticate again in a long-lived runtime. Avoid using the same rotating refresh token from concurrent processes.

### Credential-store errors

- **`credential_dir is required when credential_store is file`**: supply `--credential-dir` or the TOML key.
- **`credential_dir requires credential_store to be file`**: remove the directory setting or select `file`.
- **`OAuth credential directory must be a real directory, not a symlink`**: select a real private directory.
- **`OAuth credential path must be a regular file`**: remove the conflicting symlink or non-file entry.
- **`OAuth credential file is accessible by other users`**: on Unix, restrict the file to its owner (for example, mode `0600`) before retrying.
- **Credential-store read/write failures**: confirm the operating-system store is available and unlocked. On macOS, also confirm that the installed binary has the expected stable signing identity.
