# TUI Interaction, Sessions, and Recovery

Kit's terminal UI is an ACP client backed by a persisted session. It supports prompt editing, turn cancellation, transcript and tool-output navigation, fresh or resumed conversations, and automatic or manual context compaction. The session ID appears in the TUI header. Run `kit --help` and `kit <command> --help` for the current, exhaustive command-line options.

## Start or resume the terminal UI

Start an interactive session at a project root with the installed binary:

```sh
kit tui --root /path/to/project
```

Resume the ID shown in the header:

```sh
kit tui --root /path/to/project --resume <session-id>
```

A session ID must be 1–128 ASCII letters, digits, `-`, or `_`. `kit prompt` uses the same durable sessions: it prints `session_id: <id>` after its answer, and that ID can be continued by either `kit prompt --resume <session-id>` or `kit tui --resume <session-id>`.

## TUI keys, prompt editing, and navigation

| Key or input | Action |
| --- | --- |
| `Enter` | Send a non-empty prompt when idle |
| `Shift+Enter`, `Option+Enter`, `Ctrl+J` | Insert a newline |
| `Esc` | Interrupt a running turn; dismiss a notice when idle |
| `Ctrl+C` | Interrupt a running turn; clear a non-empty idle prompt; quit when idle with an empty prompt |
| `Ctrl+D` | Quit when the prompt is empty |
| `Option+Left/Right`, `Ctrl+A`/`Ctrl+E`, `Home`/`End` | Move by word or to the start/end of a line |
| `Option+Backspace`, `Ctrl+W` | Delete the previous word |
| `Command+Backspace`, `Ctrl+U` / `Ctrl+K` | Delete to line start / end |
| `Up` / `Down` | Move through prompt lines, then prompt history |
| `Shift+Up` / `Shift+Down`, `PageUp` / `PageDown`, mouse wheel | Scroll the transcript |
| `Ctrl+Home` / `Ctrl+End` | Jump to transcript top / bottom |
| `Ctrl+G` / `Ctrl+L` / `Ctrl+T` | Toggle the runtime graph / agent log / reasoning |
| `Ctrl+K` | Kill the selected running background tool call; otherwise delete to the end of the editor line |
| `Ctrl+O`, or click a tool card | Fold or unfold raw tool output |
| `Ctrl+Y` | Copy the latest agent response as original Markdown |
| Click a fenced code block | Copy its contents without the backticks or language label |

`Command` and `Shift+Enter` require a terminal with the Kitty keyboard protocol, such as Ghostty, Kitty, WezTerm, or recent iTerm2. The control-key alternatives work without that protocol.

`Ctrl+Y` copies with the terminal's OSC 52 clipboard protocol. It preserves the agent's original Markdown, whitespace, and newlines instead of copying rendered TUI borders, list glyphs, or wrapped lines. Clipboard access must be enabled in the terminal; multiplexers such as tmux may also require OSC 52 passthrough.

Pasted text is inserted rather than sent. Bracketed paste is used when available; otherwise Kit treats a rapid key burst as a paste, so returns in that burst become line breaks. This keeps a multiline paste in one prompt. Press plain `Enter` afterward to submit it.

### Attach local images and audio

Drag one or more supported local media files into the terminal while editing a prompt. Terminals deliver a drop as pasted, shell-escaped paths rather than as a dedicated file-drop event. Kit treats the paste as attachments only when every parsed token resolves to a supported regular file. Mixed text and paths, unsupported files, invalid shell quoting, missing files, and ambiguous input remain ordinary pasted text. There is no `/attach` command.

Supported files are PNG, JPEG, GIF, WebP, WAV, and MP3. Up to 8 attachments can be pending, each file can be at most 10 MiB, and their combined size can be at most 20 MiB. Kit checks the actual file sizes again when the prompt is submitted.

An accepted file appears in the editor as `[Image #N]` or `[Audio #N]`. Add surrounding prompt text normally, or delete a placeholder to omit that file from submission. A new session clears pending attachments. If Kit cannot read or validate the files when constructing the request, it shows a notice and retains the pending attachment records. Restore or re-enter the prompt placeholders before retrying.

The model-facing prompt retains canonical `file://` Markdown links, while Kit also reads and sends the file bytes because remote providers cannot access local files. Image and audio acceptance remains model-dependent. Kit supports these request shapes through OpenRouter and OpenAI subscription; an individual model can still reject a modality it does not support. Video is not supported.

Assistant- and tool-produced media appears as portable Markdown placeholders or links. Kit does not render images in the terminal or play audio. Only bounded `file://`, `http://`, and `https://` links are displayed; base64 and `data:` URLs are never copied into terminal text or Markdown links.

### Interrupt a running turn or quit

Press `Esc` or `Ctrl+C` once to request cancellation. The TUI shows `interrupting the turn`, then records `turn interrupted` when cancellation completes. Sending another prompt while work is active is refused with `a turn is already running — esc interrupts it`.

If a turn does not stop, press `Ctrl+C` again while Kit is cancelling to leave the TUI and terminate its agent child. On normal exit during a turn, Kit first requests cancellation and briefly allows the turn to unwind so tool outcomes can be persisted, then closes the session and releases its lock.

Interrupting a turn does not stop detached background calls. Select a running background tool card and press `Ctrl+K` to kill only that call; the selected title is accented and shows `^k kill`. When a background result starts an autonomous agent continuation, the TUI displays it as an active turn, and `Esc` or `Ctrl+C` interrupts it normally.

At an idle, non-empty editor, `Ctrl+C` clears the prompt instead of unexpectedly discarding it and quitting in one step; press it again with the empty editor to quit.

## Start a new session and compact from the TUI

The TUI recognizes exact leading slash-command tokens:

```text
/new
/new Start by reviewing the tests
/compact
/compact Continue with the migration
/model
/effort
/effort high
```

`/new` closes the current session and starts a fresh persisted session. It clears the visible transcript but does not delete or alter the previous session, which remains resumable by its ID. Text following `/new` becomes the new session's first prompt.

`/model` opens the model selector. `/effort` opens the advertised ACP reasoning-effort selector; `/effort default|low|medium|high` selects directly. In either dialog, Tab toggles saving the selection to `~/.kit/config.toml`, Enter selects, and Esc closes. Saving `default` removes top-level `reasoning_effort`; other values update it without replacing unrelated TOML. A new or resumed process starts from the resolved CLI/TOML default unless the selection was saved.

`/compact` requests compaction without starting an ordinary model turn. Used alone, it ends after compaction. Text following `/compact` is retained as the latest user message and starts the next turn after compaction. Commands are recognized only at the start of the input and only as exact tokens; `/newer`, a space before `/new`, and unknown slash commands are sent to the model unchanged.

## Persisted transcripts and session files

Kit stores durable JSONL transcripts, locks, and session-associated fatal error logs in:

```text
~/.kit/sessions/<session-id>.jsonl
~/.kit/sessions/<session-id>.lock
~/.kit/errors/<session-id>/<event-id>.json
```

Fatal error records use their own versioned JSON schema and are not transcript content. They contain bounded, allowlisted diagnostics rather than prompts, tool arguments, response bodies, credentials, or URLs. Files are written atomically with owner-only permissions on Unix, and Kit retains the newest 50 records per session. Cancellation is not a fatal error and does not create a record. When persistence succeeds, local prompt and ACP terminal errors include the log path; A2A records stay server-local.

`HOME is unset; cannot locate durable sessions` means Kit cannot determine this directory. Set `HOME` to the intended home directory before starting Kit.

Transcript records are versioned and have consecutive generations. Normal items are appended and synced to disk before they are accepted into the in-memory conversation. Operations such as compaction append a replacement record; older records remain in the JSONL file, but readers treat the latest valid replacement as the canonical transcript.

Older sessions under `<root>/.kit/sessions` remain readable. The first resume validates and copies a legacy transcript into `~/.kit/sessions`; a live legacy lock produces `legacy session is actively locked by another Kit instance ...; stop it before resuming with this Kit version`. When both locations contain the ID, the global transcript is preferred.

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

Automatic compaction drops historical reasoning, summarizes an older prefix into a structured coding checkpoint, and preserves bootstrap `System` and `Context` items plus a recent tail targeting approximately 8,000 tokens (smaller for short context windows or large bootstrap instructions). An indivisible recent item or tool round can exceed that target so call/result pairs remain valid. The split never separates a tool call from its result, and oversized retained tool outputs are truncated to keep the continuation usable. Later checkpoints fold in the previous checkpoint while preferring newer facts. Manual `/compact` uses the same retention policy; optional text after the command becomes the next user input. A successful durable-session compaction is appended as a canonical transcript replacement, so resuming uses the compacted history. The TUI shows the lifecycle and adds `context compacted` after a real replacement.

Compaction uses the selected model to produce the summary and can fail or be cancelled like other model work. An error such as `compaction agent returned an empty summary` leaves the previous transcript canonical; resolve the provider problem and retry `/compact`.

## Transcript repair and crash recovery

Kit automatically repairs one specific stranded-transcript condition: a stored tool call with no surviving tool result. On load it synthesizes an error result directly after each unanswered call. On resume, while holding the session lock, it also persists the repair so later resumes see a valid call/result pair. The synthetic result says that the work may or may not have completed, so inspect project state before retrying a tool or assuming its side effects occurred.

This repair does not hide general JSONL damage. Errors including `invalid transcript line`, `unsupported session schema version`, `invalid session identity or generation`, or `transcript line ... must contain exactly one item or replacement` indicate malformed, incompatible, reordered, or edited records. Preserve a backup of the `.jsonl` file before investigating; use the session ID and line number in the diagnostic, and do not invent generations or delete arbitrary records. If no trustworthy repair is possible, start a new session and re-establish the needed context.

While a session is open, Kit can reconstruct a transcript path deleted from disk using its still-open file and can reacquire a missing lock only if no other owner won the lock. It fails closed if another process owns recovery authority. After an abnormal TUI shutdown, restarting with `--resume` should be the first recovery attempt; add `--force` only after confirming the remaining lock is stale.

### Colours on light and dark terminals

The TUI draws on the terminal's own background, so it asks the terminal for that background over OSC 11 at startup and picks a dark or light palette to match. Terminals that do not answer within 100 ms fall back to `COLORFGBG`, and then to the dark palette.

Windows builds do not query the terminal at all: they use `KIT_THEME` if it is set, and the dark palette otherwise.

Set `KIT_THEME=light` or `KIT_THEME=dark` to override the detection — useful under multiplexers or remote sessions that report the wrong background, or answer for a different terminal than the one in front of you. Any other value, including unset, keeps automatic detection.

### TUI startup and terminal recovery

The TUI runs a `kit serve` child. If that child exits before opening the session—for example because the root is missing, credentials are unavailable, or an A2A address is already taken—the TUI reports the child's last diagnostics. A silent or wedged child eventually reports `the agent did not answer the ACP handshake within 30 seconds`. Fix that diagnostic and restart with the same `--resume` ID when a transcript was created.

If an external hard kill leaves the shell in raw mode or mouse reporting appears as text, run `reset` (or reopen the terminal) before resuming. Prefer `Esc`, `Ctrl+C`, `Ctrl+D`, `SIGTERM`, or `SIGHUP` for normal shutdown so Kit can restore terminal modes, cancel active work, close the session, and clean up only locks proven stale.
