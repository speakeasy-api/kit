# Agent Plugins

Kit can load Agent Plugin packages from a local directory or a checksum-pinned online archive. Source resolution happens at startup. Kit uses `agentkit-plugins` to validate the resolved package, exposes its valid Agent Skills through the existing `skill` tool, and registers its supported MCP servers. A plugin-only configuration works without `--mcp-config` or `mcp_config`.

## Configure a source

Add a named table to `~/.kit/config.toml`:

```toml
[plugins.local-plugin]
source = "path"
path = "./plugins/local-plugin"

[plugins.remote-plugin]
source = "archive"
url = "https://github.com/owner/repo/archive/refs/tags/v1.2.0.tar.gz"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
subdir = "optional/plugin/path"
```

Aliases must contain 1–64 lowercase ASCII letters, digits, or single hyphens, with an alphanumeric first and last character. Duplicate plugin manifest names are an error.

A relative `path` is resolved against Kit's working directory, not the configuration directory. An absolute path is used directly. Local packages are validated on every startup and remain mutable local content.

For an `archive`, provide the final archive URL. Kit does not clone Git repositories, translate forge URLs, select branches or tags, or automatically update plugins. `sha256` is mandatory and identifies the exact downloaded bytes. HTTPS is required, except that explicit loopback HTTP URLs are accepted for local testing. Redirects from HTTPS must remain HTTPS. URL credentials and fragments are rejected.

Kit recognizes ZIP, gzip-compressed tar, and plain tar by content. Archives may contain `plugin.json` at the extraction root or one top-level directory, as forge-generated archives commonly do. `subdir`, when present, is applied below that selected base. Archive paths must be contained relative paths; links, special files, duplicate normalized paths, and extraction-limit violations are rejected. Executable mode bits are not preserved in this release.

## Plugin MCP servers

Kit supports the Agent Plugin `stdio` and `streamable-http` transports. The deprecated `sse` transport is not supported: Kit skips each SSE server and writes a diagnostic containing the plugin alias and server name to stderr. Other valid servers in that plugin remain available.

For `stdio`, Kit materializes the validated portable declaration as follows:

- A `command` beginning with `./` is resolved under the canonical plugin root. A bare command uses normal executable lookup.
- Every occurrence of `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` in each argument and plugin-supplied environment value is replaced with the corresponding absolute path. Kit also sets the `PLUGIN_ROOT` and `PLUGIN_DATA` environment variables; a plugin declaration cannot replace those two variables.
- An omitted `cwd` leaves the transport default unchanged, so the process inherits Kit's working directory. A `cwd` beginning with `./` is relative to the plugin root. `${PLUGIN_ROOT}` and `${PLUGIN_DATA}`, alone or followed by a validated contained suffix, select those roots. Kit creates missing data-rooted working directories.

`PLUGIN_ROOT` is the validated package directory. `PLUGIN_DATA` is a persistent per-manifest directory created under:

```text
~/.kit/plugin-data/<plugin-manifest-name>
```

The location follows the loaded Kit configuration directory, so a config loaded from another directory uses that directory's `plugin-data`. Streamable HTTP uses the validated URL and headers as declared; stdio placeholders are not expanded in HTTP URLs or headers. Plugin HTTP declarations do not add the explicit MCP file's `description`, bearer-token, or OAuth fields.

Supported MCP server names must be unique across plugins. If two plugins declare the same supported server name, startup fails and identifies both aliases. A same-named entry in configured, project-local, or command-line MCP JSON intentionally overrides the plugin server. Kit live-reloads every named file before `tool_search` and `auth`; changing an override replaces it, and removing it restores the next lower configured or plugin server without restarting Kit. An invalid file edit fails the current call and preserves the last valid combined configuration.

## Cache and startup behavior

Archive content is downloaded, checked against `sha256`, validated, and extracted atomically under:

```text
~/.kit/plugin-cache/<lowercase-sha256>
```

The cache is local state protected by the permissions of `~/.kit`; it is not a sandbox or a publisher-identity check. Remove a damaged cache entry to force a verified download. The configured checksum proves archive-byte integrity, not who published those bytes.

Resolution or package-validation failures stop startup. Non-fatal package diagnostics are written to stderr with the plugin alias. Supported validated MCP declarations are registered and begin connecting in the background at startup; unsupported SSE declarations produce the skip diagnostic described above.

Skill collision precedence is project skills, then user skills, then plugins in lexical alias order. Only immediate valid plugin skill directories approved by the package validator are exposed; nested `SKILL.md` files are not recursively added.

`serve`, `acp`, and `prompt` resolve plugins directly. `tui` validates them before launch, and its built-in Kit server reloads the same global configuration, cache, and plugin MCP declarations. Nested built-in `acp.kit` children receive Kit's configured and explicit MCP paths, project root, and credential settings. They rediscover project `.mcp.json` and reload plugins from the same global Kit configuration, preserving the full MCP precedence order. External ACP profile processes receive standard ACP traffic but do not inherit Kit plugin declarations or Kit MCP configuration unless that external program implements and configures its own equivalent behavior.
