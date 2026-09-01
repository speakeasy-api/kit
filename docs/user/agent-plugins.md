# Agent Plugins

Kit can load Agent Plugin packages from a local directory, a checksum-pinned online archive, or a Git repository. Source resolution happens at startup. Kit uses `agentkit-plugins` to validate the resolved package, exposes its valid Agent Skills through the existing `skill` tool, and registers its supported MCP servers. A plugin-only configuration works without `--mcp-config` or `mcp_config`.

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

[plugins.git-plugin]
source = "git"
url = "https://plugins.example.com/marketplace/opaque-id.git"
rev = "main"
subdir = "agent-plugins/example"
```

Aliases must contain 1–64 lowercase ASCII letters, digits, or single hyphens, with an alphanumeric first and last character. Duplicate plugin manifest names are an error.

A relative `path` is resolved against Kit's working directory, not the configuration directory. An absolute path is used directly. Local packages are validated on every startup and remain mutable local content.

For an `archive`, provide the final archive URL. Kit does not translate forge URLs or automatically update archive sources. `sha256` is mandatory and identifies the exact downloaded bytes. HTTPS is required, except that explicit loopback HTTP URLs are accepted for local testing. Redirects from HTTPS must remain HTTPS. URL credentials and fragments are rejected.

Kit recognizes ZIP, gzip-compressed tar, and plain tar by content. Archives may contain `plugin.json` at the extraction root or one top-level directory, as forge-generated archives commonly do. `subdir`, when present, is applied below that selected base. Archive paths must be contained relative paths; links, special files, duplicate normalized paths, and extraction-limit violations are rejected. Executable mode bits are not preserved in this release.

For `git`, `url` is required, while `rev` and `subdir` are optional. When `rev` is omitted, Kit fetches the remote's `HEAD`, which selects its default branch. `rev` can be an exact 40-hex SHA-1 commit ID or a safe Git ref name such as `main`, `v1.2.0`, `refs/heads/main`, or `refs/tags/v1.2.0`. Git options, refspecs, revision expressions, control characters, and malformed ref names are rejected. Kit fetches the selected name into a private Kit-controlled ref, resolves it to a full commit ID, and uses that immutable ID for archive validation and the cache. A full commit is the reproducible choice; omitted or named revisions are fetched again on every startup and can select a new commit when the remote ref moves. The URL must be an absolute HTTPS URL without user information, a query, or a fragment. Local, SCP-like, SSH, `git`, file, and external-helper transports are rejected.

Kit invokes the installed `git` executable without a shell. It preserves normal system and user Git configuration so configured noninteractive HTTPS credential helpers can authenticate private repositories, but it disables Git terminal and configured askpass prompts, sets standard GUI credential-helper controls to noninteractive, and rejects credentials in the configured URL. Before network access, Kit verifies that `url.*.insteadOf` configuration did not rewrite the validated origin. System and uncommitted attribute files are disabled with controlled empty files; committed `.gitattributes` remains effective. Git diagnostics and configured URLs are not included in errors or written to temporary files. The selected portable `subdir` is archived with literal path semantics. Kit does not check out a worktree or run repository hooks, filters, Git LFS, or submodules; symlinks and submodules in the selected tree are rejected.

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

Git packages are fetched into an isolated bare staging repository, validated, and atomically published under:

```text
~/.kit/plugin-cache/git-v1/<sha256-url>/<resolved-commit>-<sha256-subdir>/repo
```

A validated full-commit cache entry can be reused without network access. Tags are still resolved remotely on every startup before a matching resolved-commit cache entry is reused. Concurrent publishers can duplicate fetch work, but publication remains atomic: losers validate the completed winner and attempt to remove their own staging directories. Kit also removes only exactly named staging directories under the relevant cache key when they are at least 24 hours old; it does not age out published cache entries. Git commands have a 120-second timeout, hard-bounded pipe output, backoff-based live object-store checks, and a final 256 MiB object-store validation. Git archives stream directly into bounded hardened tar extraction.

The cache is local state protected by the permissions of `~/.kit`; it is not a sandbox or a publisher-identity check. Remove a damaged cache entry to force a verified download. The configured archive checksum proves archive-byte integrity, while a Git commit identifies repository content; neither proves who published it.

Resolution or package-validation failures stop startup. Non-fatal package diagnostics are written to stderr with the plugin alias. Supported validated MCP declarations are registered and begin connecting in the background at startup; unsupported SSE declarations produce the skip diagnostic described above.

Skill collision precedence is project skills, then user skills, then plugins in lexical alias order. Only immediate valid plugin skill directories approved by the package validator are exposed; nested `SKILL.md` files are not recursively added.

`serve`, `acp`, and `prompt` resolve plugins directly. `tui` validates them before launch, and its built-in Kit server reloads the same global configuration, cache, and plugin MCP declarations. Nested built-in `acp.kit` children receive Kit's configured and explicit MCP paths, project root, and credential settings. They rediscover project `.mcp.json` and reload plugins from the same global Kit configuration, preserving the full MCP precedence order. External ACP profile processes receive standard ACP traffic but do not inherit Kit plugin declarations or Kit MCP configuration unless that external program implements and configures its own equivalent behavior.
