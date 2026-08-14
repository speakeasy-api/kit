# Kit

Kit is a small, directory-rooted coding-agent runtime.

It exposes:

- ACP over stdio for clients and editors
- A2A v1 JSON-RPC for agent collaboration
- a Ratatui ACP client
- one model-visible tool: `compose`, backed by released Runlet
- hidden compose children for shell commands, hunk edits, local subagents, and A2A calls
- ChatGPT Pro through an existing Codex login

It intentionally has no permissions framework, provenance ledger, rollback system,
control-plane authentication, persistence layer, or web UI.

## Run

Authenticate with Codex once, then start the TUI:

```sh
codex login
cargo run -- tui --root /path/to/project
```

Run the headless ACP/A2A server directly:

```sh
cargo run -- serve --root /path/to/project --a2a 127.0.0.1:7331
```

`serve` reserves stdout for ACP. Diagnostics go to stderr. A2A discovery is
available from the HTTP server's Agent Card endpoint.

Kit reads `$KIT_CODEX_AUTH`, `$CODEX_HOME/auth.json`, or `~/.codex/auth.json`, in
that order. Credential login and refresh remain Codex's job.

## Deliberate limits

The root is a working directory, not a sandbox. Shell commands can access the
host. Edits are atomic per file but multi-file changes are not transactional.
Subagents are in-process, share the same root, and are bounded to depth two.
