# Migrate from Claude Code or Codex

Kit does not have a migration slash command or a fixed importer. Source harness formats change, and a useful migration usually needs choices about global versus project scope. Start Kit in the project that you want to migrate and ask the agent to inspect, translate, and validate the relevant configuration. This guide is bundled into Kit so the agent can search it with `docs`.

## Start an assisted migration

Run:

```sh
kit init
kit auth login openai
kit tui --root /path/to/project
```

This example uses Kit's default OpenAI subscription provider. If you selected OpenRouter or Speakeasy, authenticate that provider instead.

Then use a prompt that states the sources and safety boundaries:

> Read the bundled "Migrate from Claude Code or Codex" guide. Inspect my Claude Code and Codex project instructions, skills, and MCP configuration. Propose a mapping into Kit, preserve every source file, do not copy or print credentials, and ask before replacing or merging an existing Kit file. After I approve the plan, make the changes and validate the MCP status.

You can narrow the request:

- “Bring only this project's Claude Code MCP servers into Kit's project `.mcp.json`.”
- “Merge the useful instructions from `CLAUDE.md` into `AGENTS.md`; show conflicts first.”
- “Find my Codex MCP servers and convert them to Kit's global MCP JSON. Do not copy tokens or environment values that contain secrets.”
- “Configure the Claude ACP adapter as an `acp.claude` designer subagent, with `designer` mapped to Opus. Keep my Claude Code files as migration sources, and do not change the default Kit harness.”

The agent can read files outside the selected project when the Kit process has operating-system access to them. Keep the request scoped, and review proposed changes to home-directory configuration before approval.

## Destination files in Kit

| Concern | Kit destination | Notes |
| --- | --- | --- |
| Runtime, provider, model, and external ACP harnesses | `~/.kit/config.toml` | Command-line flags override this file. Authenticate providers and external harnesses separately. |
| MCP servers for every project | The JSON file named by `mcp_config` in `~/.kit/config.toml`; `kit init` defaults it to `~/.kit/mcp.json` | The file uses a top-level `mcpServers` object. |
| MCP servers for one project | `<project>/.mcp.json` | Kit discovers this file from the canonical `--root`. Project entries override same-named global entries. |
| Project instructions | `AGENTS.md` at the project root or an ancestor | Kit loads the chain of `AGENTS.md` files from the root and its ancestors. |
| Agent Skills | `<project>/.agents/skills` or `~/.agents/skills` | Each compatible skill is a package containing `SKILL.md`. Inspect scripts and dependencies before copying it. |

Do not migrate provider tokens, OAuth caches, `auth.json`, Keychain entries, or another harness's credential files. Use `kit auth login openai`, `kit auth login openrouter`, or `kit auth login speakeasy` for Kit providers. A remote MCP server that challenges with OAuth appears as `authentication_required`; ask the agent to authenticate it in `kit tui`, `kit serve`, or `kit acp`.

## Claude Code mapping

Claude Code configuration can be user-scoped, project-scoped, managed, or supplied by plugins. Its exact locations and fields depend on the installed version, so the agent should inventory the active setup instead of assuming that every possible file exists. Common sources to inspect include `~/.claude.json`, project `.mcp.json`, `.claude/settings.json`, `.claude/settings.local.json`, `CLAUDE.md`, `.claude/CLAUDE.md`, and user or project skill directories.

Map compatible content as follows:

- **MCP:** extract the active `mcpServers` entries and convert only fields supported by Kit's strict MCP schema. Claude Code can expand `${VAR}` and `${VAR:-default}` placeholders in MCP values; Kit's normal JSON MCP layer does not. Detect every placeholder and either translate it to supported configuration or reject the entry with an unresolved expression. A project `.mcp.json` can remain in place only when it validates and has no incompatible expansion behavior. Decide whether user-scoped servers belong in Kit's global MCP file or should become project-local. Add a specific `description` for each server.
- **Instructions:** merge durable repository guidance from Claude instruction files into the `AGENTS.md` chain for the selected root. Do not concatenate blindly: remove Claude-only directions and reconcile conflicts with existing instructions. Kit loads only the selected root and its ancestors, not descendant `AGENTS.md` files. Consolidate nested guidance when Kit runs at the repository root, or select the corresponding subproject as `--root` when that guidance should apply only there.
- **Skills:** place compatible Agent Skills under `.agents/skills` or `~/.agents/skills`. Review each package before copying executable scripts or dependencies. Claude-specific slash commands, hooks, plugin metadata, and UI settings do not automatically become Kit skills.
- **Permissions and hooks:** do not translate allowlists, approval settings, or hooks into an implied Kit sandbox. Kit is not a security boundary. Convert only useful behavioral guidance into explicit instructions, and use an operating-system or container boundary for enforcement.
- **Models and login:** choose and authenticate Kit's provider separately. Kit cannot use a Claude subscription as credentials for its built-in `acp.kit` harness. The `@agentclientprotocol/claude-agent-acp` adapter includes the Claude Agent SDK CLI and authenticates separately with a Claude subscription or Anthropic Console; that choice determines billing and usage limits.

## Codex mapping

Codex and Kit both use `AGENTS.md`, but their discovery rules differ. Inventory `~/.codex/AGENTS.md`, `AGENTS.override.md`, and the files that Codex loads from the repository root down to its working directory. Kit does not recognize `AGENTS.override.md`; it loads `AGENTS.md` only at the selected root and its ancestors. Merge global or override guidance into the appropriate Kit `AGENTS.md`. Consolidate descendant guidance when Kit runs from the repository root, or select that subproject as Kit's `--root`.

For the remaining setup:

- **MCP:** inspect active `mcp_servers` tables in Codex configuration, commonly `~/.codex/config.toml` and project `.codex/config.toml`, and translate them into Kit's JSON `mcpServers` entries. Preserve command arguments and non-secret environment settings, but review transport names and omit fields that Kit does not support.
- **Skills:** compatible Agent Skills that already live in `.agents/skills` or `~/.agents/skills` are directly discoverable by Kit. For skills stored in a Codex-specific location, inspect and copy the complete skill package to the matching Kit scope.
- **Provider and model settings:** treat these as intent, not portable authentication. Select a Kit provider and model in `~/.kit/config.toml` or with command-line flags, then authenticate through Kit.
- **Approval and sandbox settings:** these have no one-to-one Kit mapping. Do not claim that a Codex sandbox or approval policy carries over. Run Kit inside a trusted security boundary.
- **Codex as a subagent:** an installed and authenticated Codex ACP adapter can be configured under `[acp.codex]`. It remains a separate harness; Kit sends ACP session traffic but does not inject Kit credentials, plugins, or MCP configuration into it.

## Validate the result

After editing, ask the agent to perform the smallest relevant checks:

1. Parse `~/.kit/config.toml` and every changed MCP JSON file without displaying secret values.
2. Ask for the compact MCP status listing and confirm that each intended server is present. Kit reloads MCP files before `tool_search` and `auth`, so a restart is not required.
3. Authenticate challenged remote servers through Kit. Recreate credentials rather than copying another harness's token cache.
4. Search for one expected MCP capability and verify that the correct server is selected.
5. Start a new Kit session when validating changed `AGENTS.md` files or skills, because startup establishes the project instruction context and skill catalog.
6. If an external ACP harness was added, run its own installation or login check first, then ask Kit to start one narrow test subagent.

See [Getting started and configuration](getting-started-and-configuration.md), [Configure and Use MCP Servers](mcp.md), [Reusable subagents and ACP harnesses](subagents-and-acp-harnesses.md), and [Security, limits, and troubleshooting](security-limits-and-troubleshooting.md) for the destination formats and runtime behavior.
