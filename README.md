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

## Terminal client

`tui` starts a `kit serve` child and drives it over ACP, so the client sees the
same protocol any editor would.

While a `compose` call runs, the right-hand pane draws its runtime graph: the
Runlet program parsed into its call, loop, branch, and boundary structure, with
each nested shell, edit, subagent, and A2A dispatch shown live under the node
that most likely issued it, including concurrent fan-out and failures. Child
call lifecycle is exact; when the same tool appears in several places, node
attribution is a stable heuristic. Nested-call
lifecycle reaches the client on stderr as marked JSON lines, enabled for the
child process with `KIT_RUNTIME_EVENTS=1`; other ACP hosts never see them.

| Key | Action |
| --- | --- |
| `⏎` | send |
| `⇧⏎`, `⌥⏎`, `^j` | newline |
| `esc` | interrupt the running turn |
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

Pasting multi-line text puts line breaks in the prompt instead of sending it.
The client asks the terminal to bracket pastes; where that is unavailable a
paste arrives as a key burst, and a return inside such a burst is read as a
line break rather than a send. Either way the whole paste is applied in one
redraw.

## Deliberate limits

The root is a working directory, not a sandbox. Shell commands can access the
host. Edits are atomic per file but multi-file changes are not transactional.
Subagents are in-process, share the same root, and are bounded to depth two.
