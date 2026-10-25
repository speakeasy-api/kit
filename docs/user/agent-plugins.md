# Agent Plugins

Use Agent Plugins to add packaged skills and MCP servers to Kit. Choose a local directory for a package you edit, a checksum-pinned archive for fixed downloaded content, or a Git repository. Kit loads plugins at startup and reloads them during a session. You do not need a separate MCP configuration file for plugin servers.

## Configure a source

Use `kit config set` to add a named table to `~/.kit/config.toml`:

```sh
kit config set 'plugins.local-plugin' '{ source = "path", path = "./plugins/local-plugin" }'
kit config get 'plugins.local-plugin'
# Remove the source when it is no longer needed:
kit config unset 'plugins.local-plugin'
```

See [configuration editing](getting-started-and-configuration.md#edit-configuration-from-the-command-line) for dotted and quoted TOML paths and value parsing. The commands edit local configuration only; plugin fetching and validation happen when Kit loads the plugins.

The equivalent local source and other source types look like this:

```toml
[plugins.local-plugin]
source = "path"
path = "./plugins/local-plugin"

[plugins.remote-plugin]
source = "archive"
url = "https://github.com/owner/repo/archive/refs/tags/v1.2.0.tar.gz"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
subdir = "optional/plugin/path"

[plugins.git-plugin]
source = "git"
url = "https://plugins.example.com/marketplace/opaque-id.git"
rev = "main"
subdir = "agent-plugins/example"
```

Aliases must contain 1–64 lowercase ASCII letters, digits, or single hyphens, with an alphanumeric first and last character. Duplicate plugin manifest names are an error.

A relative `path` is resolved against Kit's working directory, not the configuration directory. An absolute path is used directly. Local packages are validated at startup and each reload boundary; validated skill generations are snapshotted even though the source package remains mutable local content.

For an `archive`, provide the final archive URL. Kit does not translate forge URLs or automatically update archive sources. `sha256` is mandatory and identifies the exact downloaded bytes. HTTPS is required, except that explicit loopback HTTP URLs are accepted for local testing. Redirects from HTTPS must remain HTTPS. URL credentials and fragments are rejected.

Kit recognizes ZIP, gzip-compressed tar, and plain tar by content. Archives may contain `plugin.json` at the extraction root or one top-level directory, as forge-generated archives commonly do. `subdir`, when present, is applied below that selected base. Archive paths must be contained relative paths; links, special files, duplicate normalized paths, and extraction-limit violations are rejected. Executable mode bits are not preserved.

For `git`, `url` is required, while `rev` and `subdir` are optional. When `rev` is omitted, Kit fetches the remote's `HEAD`, which selects its default branch. `rev` can be an exact 40-hex SHA-1 commit ID or a safe Git ref name such as `main`, `v1.2.0`, `refs/heads/main`, or `refs/tags/v1.2.0`. Git options, refspecs, revision expressions, control characters, and malformed ref names are rejected. A full commit is the reproducible choice; omitted or named revisions are fetched again at startup and each reload boundary, and can select a new commit when the remote ref moves. The URL must be an absolute HTTPS URL without user information, a query, or a fragment. Local, SCP-like, SSH, `git`, file, and external-helper transports are rejected.

Git sources require an installed `git` executable. For private repositories, configure a noninteractive HTTPS credential helper: Kit disables terminal, askpass, and standard GUI credential prompts. URL rewriting with `url.*.insteadOf` is rejected if it changes the configured origin. Errors omit Git diagnostics and configured URLs.

Kit does not run repository hooks or filters, fetch Git LFS content, or initialize submodules. Symlinks and submodules in the selected tree are rejected. Only committed `.gitattributes` applies; system and uncommitted attribute files do not. Select a `subdir` that contains a self-contained plugin package.

## Plugin MCP servers

Kit supports the Agent Plugin `stdio` and `streamable-http` transports. The deprecated `sse` transport is not supported. Kit skips an SSE declaration, or a declaration with an unsupported `cwd`, a `./` command outside the plugin root, or an invalid header, and still loads the plugin's other servers.

For `stdio`, Kit materializes the validated portable declaration as follows:

- A `command` beginning with `./` is resolved under the canonical plugin root. A bare command uses normal executable lookup.
- Every occurrence of `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` in each argument and plugin-supplied environment value is replaced with the corresponding absolute path. Kit also sets the `PLUGIN_ROOT` and `PLUGIN_DATA` environment variables; a plugin declaration cannot replace those two variables.
- An omitted `cwd` leaves the transport default unchanged, so the process inherits Kit's working directory. A `cwd` beginning with `./` is relative to the plugin root. `${PLUGIN_ROOT}` and `${PLUGIN_DATA}`, alone or followed by a validated contained suffix, select those roots. Kit creates missing data-rooted working directories.

`PLUGIN_ROOT` is the validated package directory. `PLUGIN_DATA` is a persistent per-manifest directory created under:

```text
~/.kit/plugin-data/<plugin-manifest-name>
```

The location follows the loaded Kit configuration directory, so a config loaded from another directory uses that directory's `plugin-data`. Streamable HTTP uses the validated URL and headers as declared; stdio placeholders are not expanded in HTTP URLs or headers. Plugin HTTP declarations do not add the explicit MCP file's `description`, bearer-token, or OAuth fields.

Invoke plugin MCP tools through Kit's `tool` dispatcher, using names returned by `tool_search`. Exact `mcp_*` names are not direct compose callables.

If two plugins declare the same MCP server name, the plugin with the lexically first alias keeps it and later declarations are skipped. A skipped invalid declaration still reserves its name, so a later plugin cannot take the server over. A same-named entry in configured, project-local, or command-line MCP JSON intentionally overrides the plugin server. Kit live-reloads every named file before `tool_search` and `auth`; changing an override replaces it, and removing it restores the next lower configured or plugin server without restarting Kit. An invalid file edit fails the current call and preserves the last valid combined configuration.

## Cache and live reload behavior

Archive content is downloaded, checked against `sha256`, validated, and extracted atomically under:

```text
~/.kit/plugin-cache/<lowercase-sha256>
```

Git packages are fetched into an isolated bare staging repository, validated, and atomically published under:

```text
~/.kit/plugin-cache/git-v1/<sha256-url>/<resolved-commit>-<sha256-subdir>/repo
```

A validated full-commit cache entry can be reused without network access. Tags are resolved remotely on every startup before a matching cache entry is reused. Published cache entries do not expire automatically. Git commands have a 120-second timeout, and the fetched object store must fit within 256 MiB.

The cache is local state protected by the permissions of `~/.kit`; it is not a sandbox or a publisher-identity check. Remove a damaged cache entry to force a verified download. The configured archive checksum proves archive-byte integrity, while a Git commit identifies repository content; neither proves who published it.

Resolution or package-validation failures stop startup. In a live session, Kit rereads the exact Kit config file used at startup and re-resolves path, archive, and Git sources before `tool_search`, `auth`, and each ACP v1 or v2 user-prompt boundary. Added, changed, and removed plugin MCP servers are settled before `tool_search` returns. Plugin skill additions, skill-content or resource-inventory changes, and removals are visible to the live `skill` tool; ACP sessions announce catalog changes at the next user-prompt boundary. Reload does not create an unsolicited model turn.

If a live reload fails, fix the reported configuration or package error and retry the triggering tool call or prompt. The previous plugin MCP servers and skills remain active until a complete valid replacement is available. Invalid skills, skills directories, `mcp.json` files, and MCP server declarations are skipped individually while the rest of the package loads. Package-validation failures, such as an invalid `plugin.json`, and plugin files or data directories that are not available on disk reject the reload and keep the previous generation.

Skill collision precedence remains project skills, then user skills, then plugins in lexical alias order. Only immediate valid plugin skill directories approved by the package validator are exposed; nested `SKILL.md` files are not recursively added. Skill instructions and resource paths come from a read-only snapshot, so editing a package does not change an already loaded skill in place.

Changing a package's MCP declaration or files under its validated skill directories triggers reload detection. Changes to unrelated executables or extension files outside those directories do not force a reconnect. Archive URLs remain checksum-pinned: to use changed archive content, update the configured checksum.

Cache permissions and read-only skill snapshots protect against accidental changes, not hostile processes running as the same operating-system user. Plugins are not sandboxed.

`serve`, `acp`, and `prompt` resolve plugins directly. `tui` validates them before launch, and its built-in Kit server reloads the same global configuration, cache, and plugin MCP declarations. Nested built-in `acp.kit` children receive Kit's configured and explicit MCP paths, project root, and credential settings. They rediscover project `.mcp.json` and reload plugins from the same global Kit configuration, preserving the full MCP precedence order. External ACP profile processes receive standard ACP traffic but do not inherit Kit plugin declarations or Kit MCP configuration unless that external program implements and configures its own equivalent behavior.
