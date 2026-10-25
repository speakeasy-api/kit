# Configure and Use MCP Servers

Add Model Context Protocol (MCP) servers to let the agent use external tools and resources. Use project `.mcp.json` for project-specific servers or the global MCP file for servers you need across projects.

Kit merges servers from Agent Plugins, `mcp_config` in `~/.kit/config.toml`, `.mcp.json` in the canonical runtime root, and `--mcp-config`, in that precedence order. A higher layer replaces a whole same-named server; non-conflicting lower-layer servers remain. The project file is optional, while configured and command-line files are required when specified. Relative configured and command-line paths retain launch-directory resolution. Run `kit --help` and `kit <command> --help` for the exhaustive CLI reference.

Use `kit config set mcp_config ~/.kit/mcp.json` to select the global MCP file, `kit config get mcp_config` to inspect the saved path, or `kit config unset mcp_config` to remove the override. These commands edit `~/.kit/config.toml`, not the MCP JSON file or its server entries. See [configuration editing](getting-started-and-configuration.md#edit-configuration-from-the-command-line).

## Use a deployment-provided workload assertion

Configure a trusted server with `auth.type: "workload_identity"` to exchange a
mounted JWT without browser login, client registration, or a client secret:

```json
{
  "mcpServers": {
    "workload-service": {
      "type": "streamable-http",
      "url": "https://service.example.com/mcp",
      "auth": {
        "type": "workload_identity",
        "tokenFile": "/run/identity/token",
        "expectedIssuer": "https://auth.example.com/tenant",
        "tokenEndpoint": "https://auth.example.com/tenant/token",
        "resource": "https://service.example.com/mcp"
      }
    }
  }
}
```

Keep this configuration and token mount under deployment control. `tokenFile`
must be absolute; symlink rotation is supported. The resource must match the HTTPS
MCP URL. The defaults are `grant: "jwtBearer"` (RFC 7523) and
`endpointMode: "discovery"`: the endpoint must share `expectedIssuer`'s origin,
and Kit checks RFC 8414 metadata against both before sending an assertion.

For RFC 8693 token exchange, set `grant: "tokenExchange"`. Kit sends the JWT as
`subject_token`, declares its JWT type, and requests an OAuth access token.
The response must declare the access-token `issued_token_type` and a Bearer
`token_type`. A missing `expires_in` is allowed for this grant but disables caching;
zero, negative, or non-integer lifetimes are rejected. JWT bearer still requires
a positive lifetime. Kit renews cached tokens before expiry.

For deployments without discovery, set `endpointMode: "explicit"`, omit
`expectedIssuer`, and configure the trusted HTTPS `tokenEndpoint` directly.
This URL cannot contain credentials, a query, or a fragment. This explicitly
trusts the operator-selected endpoint without metadata verification. The token
file limit is 16 KiB in explicit mode; discovery retains its 8 KiB limit.

Both grants accept optional `audience`, `scope`, and an `extensions` object for
non-secret deployment parameters, for example `{"project_id":"example"}`.
Extension names use lowercase ASCII letters, digits, and underscores (1–64 bytes);
values are 1–1024 bytes without control characters, with at most 16 entries.
OAuth grant, token, authorization, and client-authentication fields are reserved
and cannot be overridden. Do not put credentials in extensions. Audience and
scope are each limited to 2048 bytes without control characters.

Redirects, token-POST retries, browser fallback, and saved-credential fallback
are disabled. Rejected MCP calls are not replayed. Tokens remain in process
memory and each configured server has its own acquisition cache.

Do not add static authorization or Host headers. Reusing an unchanged assertion
requires the issuer's explicit replay-policy support. After a process-memory
restore, restart Kit and resume the transcript rather than reuse resident tokens.

## Ask the agent to add a server

Kit does not provide an MCP-specific slash command. In a `kit tui` session, describe the server and the scope that you want:

> Add the Linear MCP server at `https://mcp.linear.app/mcp` for this project. Preserve the existing entries in `.mcp.json`, add a description that will help you find its tools, validate the file, and help me authenticate.

For a server that should be available in every project, ask the agent to update the file selected by `mcp_config` in `~/.kit/config.toml` instead. The agent should inspect the existing file before editing it, preserve unrelated servers, and never put a credential in the file unless you explicitly require static secret configuration. You can ask it to show the compact MCP status listing after the edit. New and changed servers load on the next `tool_search` or `auth` call without a restart.

Files are not the only installation route. An Agent Plugin can contribute MCP servers, and `--mcp-config` can add an explicit JSON layer for one launch. The rest of this guide documents each route and the strict JSON format for manual edits.

## Agent Plugin MCP configuration

Validated Agent Plugins can contribute `stdio` and `streamable-http` servers. Deprecated `sse` servers and other invalid plugin server declarations are skipped, and the plugin's remaining servers still load. When plugins collide on a server name, the lexically first plugin alias keeps the server and later declarations are skipped.

Plugin stdio declarations use the canonical plugin package as `PLUGIN_ROOT` and a persistent `<Kit-config-directory>/plugin-data/<plugin-manifest-name>` directory as `PLUGIN_DATA`. Kit injects both environment variables and replaces every occurrence of `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` in stdio arguments and plugin-supplied environment values. A `./` command is resolved beneath the plugin root. An omitted `cwd` leaves the transport default unchanged and inherits Kit's working directory; `./...` is plugin-root-relative; and `${PLUGIN_ROOT}` or `${PLUGIN_DATA}`, optionally with a validated contained suffix, selects that directory. Kit creates missing data-rooted working directories. These stdio placeholders are not expanded in streamable HTTP URLs or headers.

Higher-precedence JSON entries override same-named plugin servers. Kit reloads every named JSON layer before each `tool_search` and `auth`; removing an override restores the next lower layer in the live runtime. Creating or deleting the optional project file is detected the same way. An invalid edit fails that call while retaining the last valid combined configuration. See [Agent Plugins](agent-plugins.md) for package configuration and ACP inheritance details.

## MCP JSON configuration

The top-level key is `mcpServers`; each key beneath it is the server name shown by `tool_search` and accepted by `auth`. The JSON schema is strict: unknown fields, mixed `command`/`url` transports, and mismatched transport types are invalid. The optional `type` is `stdio` for a command server and `streamable-http` or `http` for a URL server; legacy entries without `type` remain valid. Kit validates every named layer before changing runtime state, then reloads them before each `tool_search` and `auth` call so live sessions see added, changed, and removed servers—and restored lower layers—without a restart. An invalid edit makes the current call fail but retains the last valid combined configuration so it becomes usable again when the file is repaired. Every configured server begins connecting in the background when Kit starts, using stored or provisioned machine credentials; interactive OAuth is never started implicitly. Each `tool_search` call waits for servers that are still initializing—including servers just added by a reload—before searching, so results always reflect a settled configuration. Give every server a specific `description`: it appears in listings and search results, and it is how an agent recognizes a server that still needs authentication. Use the exact query `mcp` (case-insensitive) for a compact configured-server status list. If the response cap omits tail entries, `total_servers`, `returned_servers`, and `truncated` report the omission.

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

Unless machine authentication is configured explicitly, OAuth is inferred reactively for every remote Streamable HTTP server. Kit first connects normally, records a `WWW-Authenticate` Bearer challenge, and uses that challenge for protected-resource discovery and dynamic client registration when `auth` is called. No `auth` block is required. Stdio servers never use this OAuth path.

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

## Machine OAuth credentials

Use machine OAuth to let an unattended Kit process access an HTTP MCP server without a browser login. This works with `kit prompt` as well as long-lived runtimes. Configure model-provider access separately before running these examples.

Before starting Kit, have your identity administrator register a client for the `client_credentials` grant, authorize its scopes and resource, and provision its credential file through your control plane. Kit does not register machine clients or provide a credential-bootstrap API. Keep credential contents out of prompts, ACP messages, logs, and source control.

### Connect with a provisioned client secret

1. Provision the secret at an absolute path readable only by the Kit service account, such as `/run/secrets/mcp-client-secret`. The file must contain nonempty UTF-8 text; Kit removes trailing CR/LF. Credential files must be regular files of at most 64 KiB.
2. Save this configuration as `/etc/kit/mcp-machine.json`, replacing the example URLs, client ID, scopes, and path with your administrator's values. If editing an existing MCP file instead, preserve unrelated server entries.

```json
{
  "mcpServers": {
    "service": {
      "type": "streamable-http",
      "url": "https://mcp.example.com/mcp",
      "auth": {
        "type": "client_credentials",
        "client_id": "kit-service",
        "scopes": ["tools:read"],
        "resource": "https://mcp.example.com/mcp",
        "expected_issuer": "https://identity.example.com",
        "method": {
          "type": "client_secret",
          "client_secret_file": "/run/secrets/mcp-client-secret"
        }
      }
    }
  }
}
```

3. Run Kit with that configuration:

```sh
kit prompt --mcp-config /etc/kit/mcp-machine.json \
  'Use tool_search with query mcp and report the status of service.'
```

The `service` entry should report `authenticated` and its available tool count. No browser or `auth` call is needed. If it reports `error`, use the [machine-authentication recovery steps](#recover-from-machine-authentication-errors) below.

The MCP URL, resource, and expected issuer must use HTTPS without embedded credentials or fragments. The discovered issuer must exactly match `expected_issuer`, and its token endpoint must share that issuer's HTTPS origin. Do not combine this `auth` block with `bearerToken` or an `Authorization` header.

### Connect with a provisioned private key

1. Have your administrator register the public key with the issuer and provision the matching RSA or EC private key at `/run/secrets/mcp-private-key.pem` (or another absolute path). Restrict access to the Kit service account. The same regular-file and 64 KiB limits apply; EC keys must use PKCS#8 PEM (`BEGIN PRIVATE KEY`).
2. Use the complete server configuration above, but replace its `auth.method` object with the following. Select the algorithm that matches your key and the issuer's supported algorithms.

```json
{
  "type": "private_key_jwt",
  "private_key_file": "/run/secrets/mcp-private-key.pem",
  "signing_algorithm": "RS256",
  "token_endpoint_audience": "https://identity.example.com/oauth/token"
}
```

3. Check the connection with the private-key configuration:

```sh
kit prompt --mcp-config /etc/kit/mcp-machine.json \
  'Use tool_search with query mcp and report the status of service.'
```

The `service` entry should report `authenticated`. Omit `token_endpoint_audience` unless your issuer requires an audience other than its issuer identifier. A signing failure does not fall back to a client secret or browser login; use the [recovery steps](#recover-from-machine-authentication-errors) if authentication fails.

### Machine OAuth configuration reference

Unknown fields and inline secrets are rejected. Use these exact field names, not interactive OAuth's `clientId`.

| Field | Requirement or default |
| --- | --- |
| `auth.type` | Required: `client_credentials`. |
| `auth.client_id` | Required, nonempty registered client ID. |
| `auth.resource` | Required HTTPS resource identifier authorized for the client. |
| `auth.expected_issuer` | Required HTTPS issuer identifier; exact discovery match. |
| `auth.scopes` | Array of strings; defaults to `[]`. |
| `auth.method.type` | Required: `client_secret` or `private_key_jwt`. |
| `auth.method.client_secret_file` | Required only for `client_secret`; absolute file path. |
| `auth.method.private_key_file` | Required only for `private_key_jwt`; absolute PEM file path. |
| `auth.method.signing_algorithm` | JWT only; `RS256` (default), `RS384`, `RS512`, `ES256`, or `ES384`. |
| `auth.method.token_endpoint_audience` | Optional JWT audience override; defaults to the discovered issuer. |

### Rotate machine credentials without restarting Kit

Atomically replace the provisioned file at the same path. Kit reopens it when acquiring a token, including renewal before an expired or nearly expired token is used for a tool call, or after a tool-call bearer challenge. A valid access token continues to work until renewal; replacing the file does not immediately revoke that token. Coordinate token revocation with your issuer if immediate revocation is required.

Machine tokens are not persisted by `--credential-store`; each Kit process acquires its own tokens. You do not need `--credential-store file` or `--credential-dir` for these provisioned files. Machine authentication always uses the configured method, never a refresh-token grant or interactive fallback.

### Recover from machine authentication errors

- **`machine OAuth issuer does not match expected_issuer`** or a token-endpoint-origin error: check the issuer identifier and discovery configuration with your administrator. Do not change the expected issuer merely to bypass an unexpected discovery response.
- **Credential-file read or validation error**: check the absolute path, service-account access, file type, nonempty contents, and size. For JWT, also check the key encoding and selected algorithm.
- **`machine OAuth token exchange failed`**: check client registration, secret or public-key registration, allowed scopes/resource, signing algorithm, and any required audience override with the issuer. Kit omits issuer response details from this error to avoid exposing credentials; use the issuer's administrative diagnostics.
- **`auth` reports that machine authentication is managed by provisioned credential files**: repair the configuration or provisioned file rather than starting a browser login. After an initialization failure, restart Kit with the corrected settings. After a renewal failure, repair the file and retry the tool call; inspect remote state first if the original operation might have had side effects.

## Discover, authenticate, and call MCP tools

Kit subscribes to catalog changes from connected servers and refreshes their tools in the background. Servers using MCP 2026-07-28 use acknowledged notification subscriptions; older servers retain their legacy notification path. A tool can be added, removed, or given a new input schema without restarting Kit or editing its configuration. Modern server entries include a `subscription` diagnostic with its state and, while active, the requested and acknowledged filters. A partial acknowledgment means only the accepted notifications will trigger updates.

Search and schema lookup use the last successfully published catalog while a refresh is pending. A server entry reports `catalog_stale: true` during that gap, and `catalog_error` if its last refresh failed. Kit retains the last usable tools and retries failed refreshes automatically. After a changed schema is published, request a new schema reference through `tool_search`; a reference to the previous schema no longer resolves. Routine catalog notifications do not start an assistant turn.

MCP tools are model-visible through four meta-tools rather than as an unrestricted static list. The expected workflow is:

1. Call `tool_search({ query: "issues" })`. The search reloads the configuration, waits for any servers that are still initializing, then ranks every connected server's tools globally with high precision: a tool must match the query in its name or match every query term. At most 5 tools are returned across all servers—often fewer—sorted by score and grouped by server with `available_tool_count`, `matched_tool_count`, `returned_tool_count`, and `truncated`, plus top-level `total_matched`, `total_returned`, and `truncated`. Responses are capped at 32 KiB by dropping lowest-ranked status-only server groups before lowest-ranked tools; the counts show when that happened. A server whose name strongly matches the query but which needs authentication or failed to connect is listed without tools. Use the exact query `tool_search({ query: "mcp" })` (case-insensitive) for a compact configured-server listing—name, bounded description, status, bounded optional error, and `available_tool_count` (`null` unless connected)—without any tool schemas. Compact listings drop tail servers when necessary and report `total_servers`, `returned_servers`, and `truncated`.
2. For interactive OAuth, if the selected server has status `authentication_required`, call `auth({ name: "projects" })` with its exact server name. Give the returned `url` to the user. While the flow is active, status is `pending`; the loopback browser callback expires after 10 minutes.
3. When the user completes OAuth, Kit stores the credentials, connects the server, and sends a notification to the originating ACP session. The agent resumes automatically and can search or call the newly available tools; no manual post-authentication search is required.
4. If a tool has `schema_incomplete: true`, inspect its full input schema with `tool_schema` using the returned `schema_ref` before constructing arguments (see below).
5. Invoke only a returned MCP tool name: `tool({ name: "returned_tool_name", args: { ... } })`. `args` must be an object matching that tool's advertised input schema. Calls have a 60-second deadline by default. Set the optional `timeout_seconds` field from 1 through 3600 only when the tool is expected to return after that default; for example, `tool({ name: "returned_tool_name", args: { ... }, timeout_seconds: 300 })`.

A connected server reports `authenticated`, including servers that do not need OAuth. If an already-connected OAuth server rejects a tool call with a Bearer challenge, Kit first refreshes the existing credentials and replays that tool call once. For interactive OAuth, a missing or failed refresh, a request for additional scopes, or a rejected replay falls back to the explicit `auth` workflow. For machine authentication, follow [the machine-authentication recovery steps](#recover-from-machine-authentication-errors) instead. A remote server whose HTTP response requests Bearer authentication reports `authentication_required`, whether it came from a plugin or explicit configuration and whether it has an `auth` block. Other initialization failures report `error` with a diagnostic. Calling `auth` for an unknown name reports `unknown MCP server`; calling it for a stdio server reports `is not remote`. Calling an undiscovered or unavailable tool reports `unknown MCP tool`.

## Session-owned subscriptions

Use `mcp_subscription` to watch server resources or catalogs for the current live session. Supported operations are `subscribe`, `status`, `unsubscribe`, `list_resources`, and `read_resource`; each takes a configured `server` name. The session identity comes from Kit, not tool arguments.

Subscribe before reading initial state. For example, if the server exposes `messages://inbox`:

```json
{"operation":"subscribe","server":"messages","notifications":{"resourceSubscriptions":["messages://inbox"]}}
```

Inspect the returned `requested` and `acknowledged` filters, then call `read_resource` with the same server and URI. The other filters are `toolsListChanged`, `resourcesListChanged`, and `promptsListChanged`. A new `subscribe` replaces this session's filter for that server.

Matching notifications wake an idle ACP session; busy sessions receive them after current work. The agent must read current resource or catalog state after waking. Updates are coalesced invalidations, not a durable event log; use a service's cursor-based read API when every event matters. Resource contents remain untrusted external data.

`unsubscribe` removes this session's interest and invalidates queued subscription notifications. Shared resource URIs remain subscribed while another session needs them. Closing the session retires its interests; subscriptions do not persist across Kit restarts or completed one-shot prompts. Each session can watch at most 32 resource URIs per server, with a maximum URI length of 4096 bytes.

Servers that reject modern `subscriptions/listen` with Method Not Found can still provide ordinary tools. Resource subscription requests fail explicitly rather than silently succeeding.

## Interactive OAuth availability

Interactive browser authentication is enabled in the long-lived `kit tui`, `kit serve`, and `kit acp` runtimes. It is disabled in the one-shot `kit prompt` command. A challenged server still appears as `authentication_required` so the cause is preserved, while calling `auth` reports `interactive MCP authentication requires the tui, serve, or acp command`.

`kit prompt` can still use OAuth credentials restored from a persistent store. Authenticate first in a long-lived runtime using the same MCP configuration and credential store, then run the prompt with those settings. The OAuth redirect listener binds a temporary `127.0.0.1` port, so the browser must be able to reach the local callback.

## Inspect large tool schemas

Oversized tool schemas in `tool_search` results expose a `schema_ref` and `schema_incomplete: true` instead of requiring the whole schema to fit in the search response. Treat incomplete schema metadata as a discovery summary, not a complete argument contract.

Call `tool_schema({ schema_ref: "returned_schema_ref" })` to inspect the root. Its optional fields are `pointer` (a JSON Pointer, default `""` for the root), `offset` (default `0`), and `limit` (default `16`, maximum `32`). For example:

```text
tool_schema({ schema_ref: "returned_schema_ref", pointer: "/properties", offset: 0, limit: 16 })
```

Each response is a bounded structural view with shallow children and their JSON pointers, incomplete metadata, and `next_offset` when more children remain. Use the returned pointers to descend and `next_offset` to page the same pointer. The top-level `children_incomplete` flag means this page does not contain every direct child; `incomplete` also accounts for shallow or truncated children. Each node summary has its own `incomplete` flag. Do not assume a shallow view contains the complete subtree. Arbitrary object keys are addressable: JSON Pointer escapes `~` as `~0` and `/` as `~1`; array elements use numeric index segments.

Schema `$ref` values are never expanded automatically. Inspect referenced locations explicitly when needed; for a local reference such as `#/$defs/Issue`, use the pointer `/$defs/Issue` with the same `schema_ref`.

Long descriptions are previews marked with `description_truncated`. When a `description_artifact` is provided, its `{ path, total_bytes }` identifies the full text. Read it with `artifact({ path: "returned_artifact_path" })`, then continue from the reader's `next_offset` until `eof`. Do not use a shell command to read it: artifacts can be retained only in memory. Other oversized strings (including enum values and unusually long pointers) use the same `<field>_truncated` and `<field>_artifact` convention. Read a truncated pointer's artifact before using that pointer to descend.

Schema references are validated against the session and current MCP catalog. Do not reuse references from another session or retain them as durable identifiers. If a reference is rejected after a catalog change, search again and use the new reference.

## ACP child behavior

Nested built-in `acp.kit` children receive the configured and explicit MCP paths, credential settings, and effective project root from their parent. They rediscover the project `.mcp.json` and reload plugins from the same global Kit configuration, preserving the plugin → configured → project → explicit precedence. External ACP profiles are separate programs: Kit sends them standard ACP initialization and prompt traffic, but does not inject Kit plugin declarations or Kit MCP configuration. Configure MCP separately in an external agent if it supports that behavior.

## OAuth credential stores

OpenAI and interactive MCP OAuth use one shared backend. [Machine OAuth credential files](#machine-oauth-credentials) do not use this setting. Choose it with `--credential-store` or `credential_store` in `~/.kit/config.toml`. The allowed values are `memory`, `keychain`, and `file`.

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

Persistent stores restore OpenAI credentials when Kit starts and restore MCP OAuth credentials during the background server initialization at startup—or when a later reload adds the server and the next `tool_search` or `auth` call initializes it; either path may refresh access tokens. This also works for the inferred default OAuth identity of a remote server with no `auth` block. Reuse the same backend, directory, and MCP server identity across commands. Processes sharing a credential backend coordinate token refreshes, so you can reuse the same store across Kit commands.

## Security guidance

- Use only MCP servers and executables you trust. Stdio configuration launches a local command, while remote tools send their declared arguments to an HTTP service.
- Keep MCP JSON files private when they contain `bearerToken`, custom authorization headers, client identifiers, or other sensitive values.
- Prefer OAuth or an external secret-injection strategy over committing static tokens. Do not commit file-backed OAuth credentials.
- Review tool names, descriptions, and input schemas returned by `tool_search` before calling `tool`; discovery does not make a remote action safe.
- Use a persistent credential store only when persistence is needed. The default memory store minimizes credentials left on disk.

## Troubleshooting MCP configuration and connections

### Configuration file errors

- **`could not read MCP config ...`**: verify the configured `mcp_config` or `--mcp-config` path and permissions; named files are required. Root `.mcp.json` is optional; creating it takes effect on the next `tool_search` or `auth` call.
- **`invalid MCP config ...`**: validate JSON syntax, the exact `mcpServers` spelling, field types, and field names such as `bearerToken`, `clientId`, and `clientMetadataUrl`. Unknown fields are rejected.
- **`MCP server names must not be empty`**, **`has an empty command`**, or **`has an empty URL`**: give every entry a non-blank name and its transport a non-blank `command` or `url`.
- **Plugin server missing**: an SSE declaration, an unsupported `cwd`, a `./` command outside the plugin root, or a name already declared by a lexically earlier plugin alias leaves that server unavailable. Migrate SSE to `streamable-http` and fix or rename the declaration.
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

## ACP session MCP configuration

Use session-scoped MCP servers when an ACP client needs tools or credentials for
one session without changing the host's MCP files. ACP v2 advertises
`capabilities.session.mcp.stdio` and `.http`. Supply `mcpServers` with
`session/new` or cold `session/resume` restoration; for example, a Streamable HTTP entry is:

```json
{
  "type": "http",
  "name": "project-tools",
  "url": "https://mcp.example.com/mcp",
  "headers": [{ "name": "Authorization", "value": "Bearer REPLACE_ME" }]
}
```

Replace the example URL and token with your server's values. These settings and
credentials remain in memory, not in MCP files or transcripts. Supply them again
when restoring a stopped session. Legacy SSE MCP transport is not supported.
For gateway host execution and reattachment rules, see [the gateway guide](gateway.md#start-the-host).

When reconnecting to a resident gateway session, omit `mcpServers` or send `[]`
to retain its current overlay. A nonempty list is rejected with guidance to use
`_kit/session/update_mcp`; reconnect does not replace or clear the overlay.

### Use MCP servers that require exclusive resources

ACP v2 sessions have separate MCP connections, including connections to host and
plugin servers. If a stdio server requires an exclusive lock or fixed port, do
not use it concurrently from multiple sessions in one raw ACP v2 process. Use
separate resource settings per session, or a shared HTTP MCP service designed
for concurrent clients. The gateway runs one child per session, but multiple
children can still compete for the same host-wide lock or port.

For singleton stdio servers, prefer `kit acp --protocol-version 2` (including
the gateway child path), which defers unused base connections. Mixed-protocol
`kit serve` retains eager base initialization for its other protocols; its ACP
v2 session can compete with that base connection. Use a concurrent HTTP service
in that deployment instead.

### Add or update a live session's MCP servers

To add client tools or rotate credentials without restarting the model session,
check `capabilities.session.mcp._meta["kit/updateMcp"]` for Kit's extension:

```json
{ "method": "_kit/session/update_mcp", "upsert": true }
```

Send that JSON-RPC method with `params` containing `sessionId` and `mcpServers`.
Both fields are required. An optional ACP `_meta` object is accepted. New client-owned names are added. An existing
client-owned name updates only that server's configuration and credentials.
Omitted servers remain registered; `[]` is a no-op. There is no removal operation.
Host and plugin servers and built-in tools are not replaced by client entries.

The response is `{}` after the batch succeeds. Duplicate names within the batch,
invalid transports or configuration, and collisions with host or plugin names
reject the whole batch without changing the client configuration. Correct the
batch and retry. Do not send credentials with each prompt.

Changes wait for in-flight operations on affected servers before updating their
connections and credentials. Unrelated registrations and background tasks remain.
Await the response before starting work that requires the updated credentials.
A client disconnect or request cancellation after reconciliation begins does not
roll the update back; retry the same batch to establish the desired configuration.
