# TUI Interaction, Sessions, and Recovery

Kit's bundled terminal UI is an ACP v2 client backed by a persisted session. It supports prompt editing, active-turn steering, turn cancellation, transcript and tool-output navigation, fresh or resumed conversations, and automatic or manual context compaction. The session ID appears in the TUI header. Run `kit --help` and `kit <command> --help` for the current, exhaustive command-line options.

## Start or resume the terminal UI

Start an interactive session at a project root with the installed binary:

```sh
kit tui --root /path/to/project
```

List sessions for the workspace, then resume the ID shown in the header or catalog:

```sh
kit sessions --root /path/to/project
kit sessions rename <session-id> "OAuth token bug" --root /path/to/project
kit sessions rename <session-id> --clear --root /path/to/project
kit tui --root /path/to/project --resume <session-id>
```

The catalog requires an existing directory and is workspace-filtered and newest-first. It reports each durable top-level session ID and updated time; sessions created as subagents are omitted based on structured origin metadata in their initial transcript. Sessions created before Kit recorded that metadata remain visible. Filtering affects discovery only; a known omitted ID can still be resumed explicitly. The generated title comes from the earliest retained useful user text so compaction does not rename a session; the preview describes the current retained history. A custom display name overrides that title in CLI, TUI, and ACP listings without changing the immutable session ID or preview. Names may contain Unicode, are trimmed, may be at most 100 characters, and may not contain line breaks or terminal control characters. Use `--clear` to restore the generated title.

A session ID must be 1–128 ASCII letters, digits, `-`, or `_`. `kit prompt` uses the same durable sessions: it prints `session_id: <id>` after its answer, and that ID can be continued by either `kit prompt --resume <session-id>` or `kit tui --resume <session-id>`.

## TUI keys, prompt editing, and navigation

| Key or input | Action |
| --- | --- |
| `Enter` | Send a non-empty prompt when idle; steer and finish the current response while active if the agent advertises that capability |
| `Shift+Enter`, `Option+Enter`, `Ctrl+J` | Insert a newline |
| `Esc` | Interrupt a running turn; dismiss a notice when idle |
| `Command+B` | Move the newest running foreground top-level compose call to the background |
| `Ctrl+C` | Interrupt a running turn; clear a non-empty idle prompt; quit when idle with an empty prompt |
| `Ctrl+D` | Quit when the prompt is empty |
| `Option+Left/Right`, `Ctrl+A`/`Ctrl+E`, `Home`/`End` | Move by word or to the start/end of a line |
| `Option+Backspace`, `Ctrl+W` | Delete the previous word |
| `Command+Backspace`, `Ctrl+U` / `Ctrl+K` | Delete to line start / end |
| `Up` / `Down` | Move through prompt lines, then prompt history |
| `Shift+Up` / `Shift+Down`, `PageUp` / `PageDown`, mouse wheel | Scroll the transcript |
| `Ctrl+Home` / `Ctrl+End` | Jump to transcript top / bottom |
| `Ctrl+R` / `Ctrl+L` / `Ctrl+T` | Toggle the agent roster / agent log / reasoning |
| `Ctrl+K` | Kill the selected running background tool call; otherwise delete to the end of the editor line |
| `Ctrl+O`, or click a tool card | Fold or unfold raw tool output |
| `Ctrl+Y` | Copy the latest agent response as original Markdown |
| Click a fenced code block | Copy its contents without the backticks or language label |

`Command` and `Shift+Enter` require a terminal with the Kitty keyboard protocol, such as Ghostty, Kitty, WezTerm, or recent iTerm2. The control-key alternatives work without that protocol.

`Ctrl+Y` copies with the terminal's OSC 52 clipboard protocol. It preserves the agent's original Markdown, whitespace, and newlines instead of copying rendered TUI borders, list glyphs, or wrapped lines. Clipboard access must be enabled in the terminal; multiplexers such as tmux may also require OSC 52 passthrough.

Pasted text is inserted rather than sent. Bracketed paste is used when available; otherwise Kit treats a rapid key burst as a paste, so returns in that burst become line breaks. This keeps a multiline paste in one prompt. Press plain `Enter` afterward to submit it.

When the session is idle, `Enter` starts a normal prompt. While the agent is active, `Enter` uses ACP v2 `steer` injection with `finish` stream behavior only when the agent advertised both capabilities. An accepted injected user message appears in the transcript as part of the current turn. If steering is unavailable, the editor keeps the message and shows `this agent does not support active steering`. Local commands and agent-advertised session commands are available only while idle.

### Attach local images and audio

Drag one or more supported local media files into the terminal while editing a prompt. Terminals deliver a drop as pasted, shell-escaped paths rather than as a dedicated file-drop event. Kit treats the paste as attachments only when every parsed token resolves to a supported regular file. Mixed text and paths, unsupported files, invalid shell quoting, missing files, and ambiguous input remain ordinary pasted text. There is no `/attach` command.

Supported files are PNG, JPEG, GIF, WebP, WAV, and MP3. Up to 8 attachments can be pending, each file can be at most 10 MiB, and their combined size can be at most 20 MiB. Kit checks the actual file sizes again when the prompt is submitted.

An accepted file appears in the editor as `[Image #N]` or `[Audio #N]`. Add surrounding prompt text normally, or delete a placeholder to omit that file from submission. A new session clears pending attachments. If Kit cannot read or validate the files when constructing the request, it shows a notice and retains the pending attachment records. Restore or re-enter the prompt placeholders before retrying.

The model-facing prompt retains canonical `file://` Markdown links, while Kit also reads and sends the file bytes because remote providers cannot access local files. Image and audio acceptance remains model-dependent. Kit supports these request shapes through OpenRouter and OpenAI subscription; an individual model can still reject a modality it does not support. Video is not supported.

User-attached images render inline as a bounded static first frame when Kit detects Kitty, Sixel, or iTerm2 graphics support. No setting is required. Unsupported terminals, malformed or oversized images, and decode failures retain the safe clickable attachment label. Animated GIF and WebP files currently show only their first frame. Assistant- and tool-produced media remains portable Markdown placeholders or links, and audio is not played. Only bounded `file://`, `http://`, and `https://` links are displayed; base64 and `data:` URLs are never copied into terminal text or Markdown links.

### Interrupt a running turn or quit

Press `Esc` or `Ctrl+C` once to request cancellation. The TUI shows `interrupting the turn`, then records `turn interrupted` when cancellation completes.

If a turn does not stop, press `Ctrl+C` again while Kit is cancelling to leave the TUI and terminate its agent child. On normal exit during a turn, Kit first requests cancellation and briefly allows the turn to unwind so tool outcomes can be persisted, then closes the session and releases its lock.

Press `Command+B` to detach the newest running foreground top-level compose call without waiting for it to finish. This shortcut requires a terminal that reports the Command key through the Kitty keyboard protocol; it has no control-key equivalent. Interrupting a turn does not stop detached background calls. Select a running background tool card and press `Ctrl+K` to kill only that call; the selected title is accented and shows `^k kill`. When a background result starts an autonomous agent continuation, the TUI displays it as an active turn, and `Esc` or `Ctrl+C` interrupts it normally.

At an idle, non-empty editor, `Ctrl+C` clears the prompt instead of unexpectedly discarding it and quitting in one step; press it again with the empty editor to quit.

## Monitor subagents in the agent roster

Press `Ctrl+R` or enter the exact local command `/agents` while idle to toggle the agent roster. The roster keeps its own visibility and scroll position. The roster covers every subagent observable to the current top-level Kit process tree, whether its call is foreground or background and regardless of the focused transcript block or tool call. Direct children are tree roots, and nested Kit descendants appear immediately beneath their parent at arbitrary depth in an always-expanded tree. Siblings retain lifecycle/creation/ID ordering within each parent, so an active child remains grouped beneath an idle parent instead of moving across subtrees. A descendant whose parent event has not arrived temporarily appears as a root with `Name · via Parent` and automatically reparents when the parent arrives. Generic ACP has no portable child-session enumeration, so agents created privately inside a generic harness cannot appear unless the harness forwards compatible Kit runtime events.

Each row uses two lines, with tree connectors and indentation continuing across both. The first contains a glyph and display name; the second contains the bounded task summary and a duration right-aligned to the panel's inner edge. When starting or forking a subagent, the parent model preferably supplies a concise role-oriented name such as `Round 2 Implementer` or `Reviewer`; omitted or invalid names fall back to `Agent N`, and case-insensitive sibling collisions receive a numeric suffix. Task text truncates before the reserved duration column. The glyph palette is yellow `Pulse::Child` for `starting`, cyan `Pulse::Tool` for `working`, and dim `○` for ordinary or successful `idle`. A failed reusable idle row shows a red `✗` for four seconds after its failure timestamp, then returns to dim `○`; a failed terminal or removed tombstone shows the red `✗` for four seconds, then its row is deleted. Active durations update on animation ticks, freeze when the generation becomes idle or fails, and restart for a later prompt.

A fixed footer remains visible while rows scroll, for example `3 agents · 2 working · 1 idle`; it includes the total and only nonzero `starting`, `working`, and `idle` buckets. Foreground and background are not separate buckets. Footer accounting remains lifecycle-based during the four-second grace: reusable failures count as idle immediately, while closed and terminally retired handles leave the live total immediately even while a tombstone remains visible. Idle rows remain until their handles are closed.

When the agent roster is visible, terminals at least 108 columns wide show the transcript beside a fixed 46-column panel. Terminals at most 107 columns wide stack the transcript and roster with a 55/45 height split. Hiding the roster restores the transcript to the full main area.

## Manage sessions and compact from the TUI

The TUI handles `/new`, `/resume`, `/sessions`, `/close`, `/model`, `/effort`, and `/agents` as exact local slash-command tokens. It also discovers agent commands through ACP and highlights them without interpreting them locally:

```text
/new
/new Start by reviewing the tests
/resume <session-id>
/sessions
/close
/compact
/compact Continue with the migration
/model
/effort
/effort high
/agents
```

These local commands are available only while the session is idle. `/agents` toggles the agent roster without starting a model turn. `/new` closes the current session and starts a fresh persisted session. It clears the visible transcript but does not delete or alter the previous session, which remains resumable by its ID. Text following `/new` becomes the new session's first prompt. `/resume <session-id>` closes the current session, resumes the requested durable session, and replays its transcript; selecting the already-active ID is a no-op. `/sessions` opens a visible newest-first selector for the same workspace. Up and Down move, Enter uses the existing resume flow, `R` opens an inline rename field, and Esc cancels renaming or closes the dialog. Submit an empty rename and confirm to clear the custom name. After a save, the picker remains open on the selected session and refreshes its displayed name. `/close` closes the current session and exits the TUI.

`/model` opens the model selector. `/effort` opens the advertised ACP reasoning-effort selector; `/effort default|low|medium|high` selects directly. In either dialog, Tab toggles saving the selection to `~/.kit/config.toml`, Enter selects, and Esc closes. Saving `default` removes top-level `reasoning_effort`; other values update it without replacing unrelated TOML. A new or resumed process starts from the resolved CLI/TOML default unless the selection was saved.

The ACP server advertises `compact` for every new session. The TUI submits `/compact` unchanged like any other prompt; the runtime consumes exactly one text part beginning with the exact raw token before model dispatch and permits other client-provided context parts. Used alone, it ends after compaction. Whitespace-trimmed text following `/compact` and any other context parts are retained as the latest user message and start the next turn after compaction. Leading whitespace, near-misses such as `/compactness`, prompts containing multiple `/compact` command parts, and unknown slash commands remain ordinary prompts. Local commands win if an advertised command has the same name.

## Persisted transcripts and session files

Kit stores durable JSONL transcripts, locks, and session-associated fatal error logs in:

```text
~/.kit/sessions/w-<workspace-hash>/<session-id>.jsonl
~/.kit/sessions/w-<workspace-hash>/<session-id>.lock
~/.kit/sessions/w-<workspace-hash>/<session-id>.metadata.json
~/.kit/errors/<session-id>/<event-id>.json
```

The workspace hash is the BLAKE3 digest of the canonical workspace-root path. It keeps identical session IDs in different workspaces in separate storage directories. The optional metadata sidecar stores only the custom display name, is replaced atomically, and does not modify or lock the append-only transcript. Missing or malformed metadata falls back to the generated title without hiding the session.

Fatal error records use their own versioned JSON schema and are not transcript content. Schema v2 adds optional structured transport diagnostics; schema v1 records remain readable. Transport diagnostics contain only bounded, allowlisted request/stream stage, retry, attempt, the provider's strictly validated `x-request-id` value, reqwest classification, and typed Hyper, HTTP/2, and I/O fields. Unknown or truncated source chains are identified without storing source text. Kit never stores raw error display/debug text, arbitrary headers, prompts, tool arguments, response bodies, credentials, URLs, or peer-controlled HTTP/2 debug text in these records. Files are written atomically with owner-only permissions on Unix, and Kit retains the newest 50 records per session. Cancellation is not a fatal error and does not create a record. When persistence succeeds, local prompt and ACP terminal errors include the log path; A2A records stay server-local.

`HOME is unset; cannot locate durable sessions` means Kit cannot determine this directory. Set `HOME` to the intended home directory before starting Kit.

Transcript records are versioned and have consecutive generations. Transcript schema v3 records the canonical workspace root so ACP discovery and resume cannot expose a session to another project; schema v1 and v2 records remain readable and gain that binding when they are next resumed. Normal items are appended and synced to disk before they are accepted into the in-memory conversation. Operations such as compaction append a replacement record; older records remain in the JSONL file, but readers treat the latest valid replacement as the canonical transcript.

Older sessions stored directly under `~/.kit/sessions` or under `<root>/.kit/sessions` remain readable. On resume, Kit compares workspace-hashed, workspace-bound global, and project-local candidates and selects the history that descends from the others; equivalent histories prefer the workspace-hashed copy, while divergent histories fail instead of choosing silently. An old global transcript without workspace metadata is a fallback only for an explicit resume when no workspace-hashed or project-local candidate has that ID. The first successful resume copies the authoritative history into the workspace-hashed directory and leaves redirects in applicable legacy files. A live legacy lock produces `legacy session is actively locked by another Kit instance ...; stop it before resuming with this Kit version`.

### ACP session loading

ACP v1 clients restore a closed durable session with `session/load` and discover sessions with the optional `session/list` capability. ACP v2 clients use `session/list` and `session/resume`. Both list variants use the same newest-first catalog, optional exact-cwd filter, and opaque `offset:<n>` pagination cursors, and return titles and RFC 3339 updated times. Both versions use the exact durable session ID and return the same model and reasoning configuration options as `session/new`.

Session discovery and restoration are isolated to the server's canonical workspace root. A requested workspace must match that root, and additional directories are not accepted. Legacy transcripts under a project-local `.kit/sessions` directory follow the same migration and root checks as CLI resume; they do not make a same-named session visible from another workspace. Old global transcripts without workspace metadata are excluded from discovery in every workspace, but an explicit resume by ID remains supported and binds the transcript to that workspace. An individually malformed or concurrently incomplete transcript is omitted from catalog results without preventing valid sessions from being listed; explicit resume remains strict and reports its error.

An arbitrary ACP load or resume never applies the server process's configured `--force` setting. The one exception is the initial resume requested by `kit tui --resume <id> --force`: only that matching configured session may use the explicit stale-lock override. If another live Kit instance owns the session lock, restoration fails instead of taking over the session. A missing or invalid ID also fails normally. After the session closes and releases its lock, an ACP client can restore it again.

Before the restoration response, Kit replays the canonical transcript as ordered ACP updates for representable user text and attachments, assistant text and thoughts, and tool calls and results. Internal instructions, ambient context, notifications, and provider-specific content are not replayed to the client, but remain in the model transcript. Because compaction replaces the canonical transcript, restoring a compacted session replays its canonical summary history rather than the superseded pre-compaction items.

### Session locks, `--resume`, and `--force`

Only one live Kit instance may mutate a session. A normal exit removes its `.lock` file. If opening a session reports:

```text
session is locked by another Kit instance (...); use --force to override a stale lock
```

first confirm that no Kit process is still using the session. Then retry the resume with:

```sh
kit tui --root /path/to/project --resume <session-id> --force
```

`--force` is only for a stale lock left by an exited or crashed process, and the CLI accepts it only with `--resume`. It does not steal a lock held by a live process: the OS-level lock check instead reports `session is actively locked by another Kit instance (...)`. Do not manually remove a lock belonging to a running Kit process.

A new session ID that already exists reports `session ... already exists; use --resume`; a missing resume target reports `session ... does not exist`. Use the correct ID and mode rather than `--force` for either error.

## Automatic context compaction

Kit checks the latest provider-reported usage. When `context_used` reaches 80% of `context_window`, it automatically compacts mutable history. If the provider did not report a context window, automatic compaction stays disabled rather than guessing a limit.

Automatic compaction drops historical reasoning, summarizes an older prefix into a structured coding checkpoint, and preserves bootstrap `System` and `Context` items plus a recent tail targeting approximately 8,000 tokens (smaller for short context windows or large bootstrap instructions). An indivisible recent item or tool round can exceed that target so call/result pairs remain valid. The split never separates a tool call from its result. Historical tool results are aggressively truncated, while the three newest results receive a larger bounded allowance. Inline media and file payloads are replaced with bounded placeholders in the summary request; durable URI and artifact references remain available to the summarizer without embedding their bytes. Each rendered item has a fixed byte limit, and the conversation prompt has a conservative byte budget that reserves at least half of the provider's context window for instructions, framing, and output. Later checkpoints fold in the previous checkpoint while preferring newer facts. Manual `/compact` uses the same retention policy; optional text after the command becomes the next user input. A successful durable-session compaction is appended as a canonical transcript replacement, so resuming uses the compacted history. The TUI shows the lifecycle and adds `context compacted` after a real replacement.

Compaction uses the selected model to produce the summary and can fail or be cancelled like other model work. An error such as `compaction agent returned an empty summary` leaves the previous transcript canonical; resolve the provider problem and retry `/compact`.

## Transcript repair and crash recovery

Kit automatically repairs one specific stranded-transcript condition: a stored tool call with no surviving tool result. On load it synthesizes an error result directly after each unanswered call. On resume, while holding the session lock, it also persists the repair so later resumes see a valid call/result pair. The synthetic result says that the work may or may not have completed, so inspect project state before retrying a tool or assuming its side effects occurred.

This repair does not hide general JSONL damage. Errors including `invalid transcript line`, `unsupported session schema version`, `invalid session identity or generation`, or `transcript line ... must contain exactly one item or replacement` indicate malformed, incompatible, reordered, or edited records. Preserve a backup of the `.jsonl` file before investigating; use the session ID and line number in the diagnostic, and do not invent generations or delete arbitrary records. If no trustworthy repair is possible, start a new session and re-establish the needed context.

While a session is open, Kit can reconstruct a transcript path deleted from disk using its still-open file and can reacquire a missing lock only if no other owner won the lock. It fails closed if another process owns recovery authority. After an abnormal TUI shutdown, restarting with `--resume` should be the first recovery attempt; add `--force` only after confirming the remaining lock is stale.

### Terminal colours and Speakeasy branding

The TUI inherits the terminal's configured foreground and background. Functional accents use ANSI colour slots, so success, warning, error, focus, and muted text follow the user's terminal palette instead of a Kit-specific dark or light theme. Selection uses the terminal's reversed style, and Kit does not probe or repaint the terminal background.

The empty starter screen adds a decorative Speakeasy rainbow line beneath the Kit name. Once a session has transcript content, the line moves beneath the header and spans the terminal width when the terminal is tall enough to show it without displacing required content. Terminals that advertise 24-bit colour receive the Speakeasy brand ramp; limited-colour terminals receive an ANSI approximation from their configured palette. No status or instruction depends on distinguishing those colours.

### TUI startup and terminal recovery

The TUI runs a `kit serve` child with ACP v2 selected explicitly for its stdio connection; ordinary `kit serve` invocations continue to default to ACP v1 on stdio. If that child exits before opening the session—for example because the root is missing, credentials are unavailable, or an A2A address is already taken—the TUI reports the child's last diagnostics. A silent or wedged child eventually reports `the agent did not answer the ACP handshake within 30 seconds`. Fix that diagnostic and restart with the same `--resume` ID when a transcript was created.

If an external hard kill leaves the shell in raw mode or mouse reporting appears as text, run `reset` (or reopen the terminal) before resuming. Prefer `Esc`, `Ctrl+C`, `Ctrl+D`, `SIGTERM`, or `SIGHUP` for normal shutdown so Kit can restore terminal modes, cancel active work, close the session, and clean up only locks proven stale.
